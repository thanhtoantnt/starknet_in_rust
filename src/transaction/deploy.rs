use std::sync::Arc;

use crate::execution::execution_entry_point::ExecutionResult;
use crate::services::api::contract_classes::deprecated_contract_class::{
    ContractClass, EntryPointType,
};
use crate::state::cached_state::CachedState;
use crate::syscalls::syscall_handler_errors::SyscallHandlerError;
use crate::{
    core::{
        contract_address::compute_deprecated_class_hash, errors::hash_errors::HashError,
        errors::state_errors::StateError, transaction_hash::calculate_deploy_transaction_hash,
    },
    definitions::{
        block_context::BlockContext, constants::CONSTRUCTOR_ENTRY_POINT_SELECTOR,
        transaction_type::TransactionType,
    },
    execution::{
        execution_entry_point::ExecutionEntryPoint, CallInfo, TransactionExecutionContext,
        TransactionExecutionInfo,
    },
    hash_utils::calculate_contract_address,
    services::api::{
        contract_class_errors::ContractClassError, contract_classes::compiled_class::CompiledClass,
    },
    state::state_api::{State, StateReader},
    state::ExecutionResourcesManager,
    transaction::error::TransactionError,
    utils::{calculate_tx_resources, felt_to_hash, Address, ClassHash},
};
use cairo_vm::felt::Felt252;
use num_traits::Zero;

use super::Transaction;

/// Represents a Deploy Transaction in the starknet network
#[derive(Debug, Clone)]
pub struct Deploy {
    pub hash_value: Felt252,
    pub version: Felt252,
    pub contract_address: Address,
    pub contract_address_salt: Felt252,
    pub contract_hash: ClassHash,
    pub contract_class: CompiledClass,
    pub constructor_calldata: Vec<Felt252>,
    pub tx_type: TransactionType,
    pub skip_validate: bool,
    pub skip_execute: bool,
    pub skip_fee_transfer: bool,
}

impl Deploy {
    pub fn new(
        contract_address_salt: Felt252,
        contract_class: ContractClass,
        constructor_calldata: Vec<Felt252>,
        chain_id: Felt252,
        version: Felt252,
    ) -> Result<Self, SyscallHandlerError> {
        let class_hash = compute_deprecated_class_hash(&contract_class).map_err(|e| {
            SyscallHandlerError::HashError(HashError::FailedToComputeHash(e.to_string()))
        })?;

        let contract_hash: ClassHash = felt_to_hash(&class_hash);
        let contract_address = Address(calculate_contract_address(
            &contract_address_salt,
            &class_hash,
            &constructor_calldata,
            Address(Felt252::zero()),
        )?);

        let hash_value = calculate_deploy_transaction_hash(
            version.clone(),
            &contract_address,
            &constructor_calldata,
            chain_id,
        )?;

        Ok(Deploy {
            hash_value,
            version,
            contract_address,
            contract_address_salt,
            contract_hash,
            contract_class: CompiledClass::Deprecated(Arc::new(contract_class)),
            constructor_calldata,
            tx_type: TransactionType::Deploy,
            skip_validate: false,
            skip_execute: false,
            skip_fee_transfer: false,
        })
    }

    pub fn new_with_tx_hash(
        contract_address_salt: Felt252,
        contract_class: ContractClass,
        constructor_calldata: Vec<Felt252>,
        version: Felt252,
        hash_value: Felt252,
    ) -> Result<Self, SyscallHandlerError> {
        let class_hash = compute_deprecated_class_hash(&contract_class).map_err(|e| {
            SyscallHandlerError::HashError(HashError::FailedToComputeHash(e.to_string()))
        })?;
        let contract_hash: ClassHash = felt_to_hash(&class_hash);
        let contract_address = Address(calculate_contract_address(
            &contract_address_salt,
            &class_hash,
            &constructor_calldata,
            Address(Felt252::zero()),
        )?);

        Ok(Deploy {
            hash_value,
            version,
            contract_address,
            contract_address_salt,
            contract_hash,
            constructor_calldata,
            contract_class: CompiledClass::Deprecated(Arc::new(contract_class)),
            tx_type: TransactionType::Deploy,
            skip_validate: false,
            skip_execute: false,
            skip_fee_transfer: false,
        })
    }

    /// Returns the class hash of the deployed contract
    pub fn class_hash(&self) -> ClassHash {
        self.contract_hash
    }

    fn constructor_entry_points_empty(
        &self,
        contract_class: CompiledClass,
    ) -> Result<bool, StateError> {
        match contract_class {
            CompiledClass::Deprecated(class) => Ok(class
                .entry_points_by_type
                .get(&EntryPointType::Constructor)
                .ok_or(ContractClassError::NoneEntryPointType)?
                .is_empty()),
            CompiledClass::Casm(class) => Ok(class.entry_points_by_type.constructor.is_empty()),
        }
    }
    /// Deploys the contract in the starknet network and calls its constructor if it has one.
    /// ## Parameters
    /// - state: A state that implements the [`State`] and [`StateReader`] traits.
    /// - block_context: The block's execution context.
    pub fn apply<S: StateReader>(
        &self,
        state: &mut CachedState<S>,
        block_context: &BlockContext,
    ) -> Result<TransactionExecutionInfo, TransactionError> {
        match self.contract_class.clone() {
            CompiledClass::Casm(contract_class) => {
                state.set_compiled_class(
                    &Felt252::from_bytes_be(&self.contract_hash),
                    contract_class.as_ref().clone(),
                )?;
            }
            CompiledClass::Deprecated(contract_class) => {
                state.set_contract_class(&self.contract_hash, &contract_class)?;
            }
        }

        state.deploy_contract(self.contract_address.clone(), self.contract_hash)?;

        if self.constructor_entry_points_empty(self.contract_class.clone())? {
            // Contract has no constructors
            Ok(self.handle_empty_constructor(state)?)
        } else {
            self.invoke_constructor(state, block_context)
        }
    }
    /// Executes the contract without constructor
    /// ## Parameters
    /// - state: A state that implements the [`State`] and [`StateReader`] traits.

    pub fn handle_empty_constructor<S: State + StateReader>(
        &self,
        state: &mut S,
    ) -> Result<TransactionExecutionInfo, TransactionError> {
        if !self.constructor_calldata.is_empty() {
            return Err(TransactionError::EmptyConstructorCalldata);
        }

        let class_hash: ClassHash = self.contract_hash;
        let call_info = CallInfo::empty_constructor_call(
            self.contract_address.clone(),
            Address(Felt252::zero()),
            Some(class_hash),
        );

        let resources_manager = ExecutionResourcesManager::default();

        let changes = state.count_actual_storage_changes();
        let actual_resources = calculate_tx_resources(
            resources_manager,
            &[Some(call_info.clone())],
            self.tx_type,
            changes,
            None,
            0,
        )?;

        Ok(TransactionExecutionInfo::new_without_fee_info(
            None,
            Some(call_info),
            None,
            actual_resources,
            Some(self.tx_type),
        ))
    }

    /// Execute the contract using its constructor
    /// ## Parameters
    /// - state: A state that implements the [`State`] and [`StateReader`] traits.
    /// - block_context: The block's execution context.
    pub fn invoke_constructor<S: StateReader>(
        &self,
        state: &mut CachedState<S>,
        block_context: &BlockContext,
    ) -> Result<TransactionExecutionInfo, TransactionError> {
        let call = ExecutionEntryPoint::new(
            self.contract_address.clone(),
            self.constructor_calldata.clone(),
            CONSTRUCTOR_ENTRY_POINT_SELECTOR.clone(),
            Address(Felt252::zero()),
            EntryPointType::Constructor,
            None,
            None,
            0,
        );

        let mut tx_execution_context = TransactionExecutionContext::new(
            Address(Felt252::zero()),
            self.hash_value.clone(),
            Vec::new(),
            0,
            Felt252::zero(),
            block_context.invoke_tx_max_n_steps,
            self.version.clone(),
        );

        let mut resources_manager = ExecutionResourcesManager::default();
        let ExecutionResult {
            call_info,
            revert_error,
            n_reverted_steps,
        } = call.execute(
            state,
            block_context,
            &mut resources_manager,
            &mut tx_execution_context,
            true,
            block_context.validate_max_n_steps,
            false,
        )?;

        let changes = state.count_actual_storage_changes();
        let actual_resources = calculate_tx_resources(
            resources_manager,
            &[call_info.clone()],
            self.tx_type,
            changes,
            None,
            n_reverted_steps,
        )?;

        Ok(TransactionExecutionInfo::new_without_fee_info(
            None,
            call_info,
            revert_error,
            actual_resources,
            Some(self.tx_type),
        ))
    }

    /// Calculates actual fee used by the transaction using the execution
    /// info returned by apply(), then updates the transaction execution info with the data of the fee.
    /// ## Parameters
    /// - state: A state that implements the [`State`] and [`StateReader`] traits.
    /// - block_context: The block's execution context.
    pub fn execute<S: StateReader>(
        &self,
        state: &mut CachedState<S>,
        block_context: &BlockContext,
    ) -> Result<TransactionExecutionInfo, TransactionError> {
        let mut tx_exec_info = self.apply(state, block_context)?;
        let (fee_transfer_info, actual_fee) = (None, 0);
        tx_exec_info.set_fee_info(actual_fee, fee_transfer_info);

        Ok(tx_exec_info)
    }

    // ---------------
    //   Simulation
    // ---------------

    /// Creates a Deploy transaction for simulate a deploy
    pub(crate) fn create_for_simulation(
        &self,
        skip_validate: bool,
        skip_execute: bool,
        skip_fee_transfer: bool,
    ) -> Transaction {
        let tx = Deploy {
            skip_validate,
            skip_execute,
            skip_fee_transfer,
            ..self.clone()
        };

        Transaction::Deploy(tx)
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use super::*;
    use crate::{
        state::cached_state::CachedState, state::in_memory_state_reader::InMemoryStateReader,
        utils::calculate_sn_keccak,
    };

    #[test]
    fn invoke_constructor_test() {
        // Instantiate CachedState
        let state_reader = Arc::new(InMemoryStateReader::default());
        let mut state = CachedState::new(state_reader, Some(Default::default()), None);

        // Set contract_class
        let contract_class =
            ContractClass::from_path("starknet_programs/constructor.json").unwrap();
        let class_hash: Felt252 = compute_deprecated_class_hash(&contract_class).unwrap();
        //transform class_hash to [u8; 32]
        let class_hash_bytes = class_hash.to_be_bytes();

        let internal_deploy = Deploy::new(
            0.into(),
            contract_class.clone(),
            vec![10.into()],
            0.into(),
            0.into(),
        )
        .unwrap();

        let block_context = Default::default();

        let _result = internal_deploy.apply(&mut state, &block_context).unwrap();

        assert_eq!(
            state.get_contract_class(&class_hash_bytes).unwrap(),
            CompiledClass::Deprecated(Arc::new(contract_class))
        );

        assert_eq!(
            state
                .get_class_hash_at(&internal_deploy.contract_address)
                .unwrap(),
            class_hash_bytes
        );

        let storage_key = calculate_sn_keccak("owner".as_bytes());

        assert_eq!(
            state
                .get_storage_at(&(internal_deploy.contract_address, storage_key))
                .unwrap(),
            Felt252::from(10)
        );
    }

    #[test]
    fn invoke_constructor_no_calldata_should_fail() {
        // Instantiate CachedState
        let state_reader = Arc::new(InMemoryStateReader::default());
        let mut state = CachedState::new(state_reader, Some(Default::default()), None);

        let contract_class =
            ContractClass::from_path("starknet_programs/constructor.json").unwrap();

        let class_hash: Felt252 = compute_deprecated_class_hash(&contract_class).unwrap();
        //transform class_hash to [u8; 32]
        let class_hash_bytes = class_hash.to_be_bytes();

        state
            .set_contract_class(&class_hash_bytes, &contract_class)
            .unwrap();

        let internal_deploy =
            Deploy::new(0.into(), contract_class, Vec::new(), 0.into(), 0.into()).unwrap();

        let block_context = Default::default();

        let result = internal_deploy.execute(&mut state, &block_context);
        assert_matches!(result.unwrap_err(), TransactionError::CairoRunner(..))
    }

    #[test]
    fn deploy_contract_without_constructor_should_fail() {
        // Instantiate CachedState
        let state_reader = Arc::new(InMemoryStateReader::default());
        let mut state = CachedState::new(state_reader, Some(Default::default()), None);

        let contract_path = "starknet_programs/amm.json";
        let contract_class = ContractClass::from_path(contract_path).unwrap();

        let class_hash: Felt252 = compute_deprecated_class_hash(&contract_class).unwrap();
        //transform class_hash to [u8; 32]
        let mut class_hash_bytes = [0u8; 32];
        class_hash_bytes.copy_from_slice(&class_hash.to_bytes_be());

        state
            .set_contract_class(&class_hash_bytes, &contract_class)
            .unwrap();

        let internal_deploy = Deploy::new(
            0.into(),
            contract_class,
            vec![10.into()],
            0.into(),
            0.into(),
        )
        .unwrap();

        let block_context = Default::default();

        let result = internal_deploy.execute(&mut state, &block_context);
        assert_matches!(
            result.unwrap_err(),
            TransactionError::EmptyConstructorCalldata
        )
    }

    #[test]
    fn internal_deploy_computing_classhash_should_fail() {
        let contract_path = "starknet_programs/amm.json";
        // Take a contrat class to copy the program
        let contract_class = ContractClass::from_path(contract_path).unwrap();

        // Make a new contract class with the same program but with errors
        let error_contract_class = ContractClass {
            hinted_class_hash: contract_class.hinted_class_hash,
            program: contract_class.program,
            entry_points_by_type: HashMap::new(),
            abi: None,
        };

        // Should fail when compouting the hash due to a failed contract class
        let internal_deploy_error = Deploy::new(
            0.into(),
            error_contract_class,
            Vec::new(),
            0.into(),
            1.into(),
        );
        assert_matches!(
            internal_deploy_error.unwrap_err(),
            SyscallHandlerError::HashError(HashError::FailedToComputeHash(_))
        )
    }
}
