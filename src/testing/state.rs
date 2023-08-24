use super::{state_error::StarknetStateError, type_utils::ExecutionInfo};
use crate::execution::execution_entry_point::ExecutionResult;
use crate::services::api::contract_classes::deprecated_contract_class::EntryPointType;
use crate::{
    definitions::{block_context::BlockContext, constants::TRANSACTION_VERSION},
    execution::{
        execution_entry_point::ExecutionEntryPoint, CallInfo, Event, TransactionExecutionContext,
        TransactionExecutionInfo,
    },
    services::api::{
        contract_classes::deprecated_contract_class::ContractClass, messages::StarknetMessageToL1,
    },
    state::{
        cached_state::CachedState,
        state_api::{State, StateReader},
    },
    state::{in_memory_state_reader::InMemoryStateReader, ExecutionResourcesManager},
    transaction::{
        error::TransactionError, invoke_function::InvokeFunction, Declare, Deploy, Transaction,
    },
    utils::{Address, ClassHash},
};
use cairo_vm::felt::Felt252;
use num_traits::{One, Zero};
use std::collections::HashMap;
use std::sync::Arc;

// ---------------------------------------------------------------------
/// StarkNet testing object. Represents a state of a StarkNet network.
pub struct StarknetState {
    pub state: CachedState<InMemoryStateReader>,
    pub(crate) block_context: BlockContext,
    l2_to_l1_messages: HashMap<Vec<u8>, usize>,
    l2_to_l1_messages_log: Vec<StarknetMessageToL1>,
    events: Vec<Event>,
}

impl StarknetState {
    pub fn new(context: Option<BlockContext>) -> Self {
        let block_context = context.unwrap_or_default();
        let state_reader = Arc::new(InMemoryStateReader::default());

        let state = CachedState::new(state_reader, Some(HashMap::new()), Some(HashMap::new()));

        let l2_to_l1_messages = HashMap::new();
        let l2_to_l1_messages_log = Vec::new();

        let events = Vec::new();
        StarknetState {
            state,
            block_context,
            l2_to_l1_messages,
            l2_to_l1_messages_log,
            events,
        }
    }

    pub fn new_with_states(
        block_context: Option<BlockContext>,
        state: CachedState<InMemoryStateReader>,
    ) -> Self {
        let block_context = block_context.unwrap_or_default();
        let l2_to_l1_messages = HashMap::new();
        let l2_to_l1_messages_log = Vec::new();

        let events = Vec::new();
        StarknetState {
            state,
            block_context,
            l2_to_l1_messages,
            l2_to_l1_messages_log,
            events,
        }
    }

    // ------------------------------------------------------------------------------------
    /// Declares a contract class.
    /// Returns the class hash and the execution info.
    /// Args:
    /// contract_class - a compiled StarkNet contract
    pub fn declare(
        &mut self,
        contract_class: ContractClass,
    ) -> Result<(ClassHash, TransactionExecutionInfo), TransactionError> {
        let tx = Declare::new(
            contract_class,
            self.chain_id(),
            Address(Felt252::one()),
            0,
            0.into(),
            Vec::new(),
            0.into(),
        )?;

        let tx_execution_info = tx.execute(&mut self.state, &self.block_context)?;

        Ok((tx.class_hash, tx_execution_info))
    }

    /// Invokes a contract function. Returns the execution info.

    #[allow(clippy::too_many_arguments)]
    pub fn invoke_raw(
        &mut self,
        contract_address: Address,
        selector: Felt252,
        calldata: Vec<Felt252>,
        max_fee: u128,
        signature: Option<Vec<Felt252>>,
        nonce: Option<Felt252>,
        hash_value: Option<Felt252>,
        remaining_gas: u128,
    ) -> Result<TransactionExecutionInfo, StarknetStateError> {
        let tx = self.create_invoke_function(
            contract_address,
            selector,
            calldata,
            max_fee,
            signature,
            nonce,
            hash_value,
        )?;

        let mut tx = Transaction::InvokeFunction(tx);
        self.execute_tx(&mut tx, remaining_gas)
    }

    /// Builds the transaction execution context and executes the entry point.
    /// Returns the CallInfo.
    pub fn execute_entry_point_raw(
        &mut self,
        contract_address: Address,
        entry_point_selector: Felt252,
        calldata: Vec<Felt252>,
        caller_address: Address,
    ) -> Result<CallInfo, StarknetStateError> {
        let call = ExecutionEntryPoint::new(
            contract_address,
            calldata,
            entry_point_selector,
            caller_address,
            EntryPointType::External,
            None,
            None,
            0,
        );

        let mut resources_manager = ExecutionResourcesManager::default();

        let mut tx_execution_context = TransactionExecutionContext::default();
        let ExecutionResult { call_info, .. } = call.execute(
            &mut self.state,
            &self.block_context,
            &mut resources_manager,
            &mut tx_execution_context,
            false,
            self.block_context.invoke_tx_max_n_steps,
            false,
        )?;

        let call_info = call_info.ok_or(StarknetStateError::Transaction(
            TransactionError::CallInfoIsNone,
        ))?;

        let exec_info = ExecutionInfo::Call(Box::new(call_info.clone()));
        self.add_messages_and_events(&exec_info)?;

        Ok(call_info)
    }

    /// Deploys a contract. Returns the contract address and the execution info.
    /// Args:
    /// contract_class - a compiled StarkNet contract
    /// contract_address_salt
    /// the salt to use for deploying. Otherwise, the salt is randomized.
    pub fn deploy(
        &mut self,
        contract_class: ContractClass,
        constructor_calldata: Vec<Felt252>,
        contract_address_salt: Felt252,
        hash_value: Option<Felt252>,
        remaining_gas: u128,
    ) -> Result<(Address, TransactionExecutionInfo), StarknetStateError> {
        let chain_id = self.block_context.starknet_os_config.chain_id.clone();
        let deploy = match hash_value {
            None => Deploy::new(
                contract_address_salt,
                contract_class.clone(),
                constructor_calldata,
                chain_id,
                TRANSACTION_VERSION.clone(),
            )?,
            Some(hash_value) => Deploy::new_with_tx_hash(
                contract_address_salt,
                contract_class.clone(),
                constructor_calldata,
                TRANSACTION_VERSION.clone(),
                hash_value,
            )?,
        };
        let contract_address = deploy.contract_address.clone();
        let contract_hash = deploy.contract_hash;
        let mut tx = Transaction::Deploy(deploy);

        self.state
            .set_contract_class(&contract_hash, &contract_class)?;

        let tx_execution_info = self.execute_tx(&mut tx, remaining_gas)?;
        Ok((contract_address, tx_execution_info))
    }

    pub fn execute_tx(
        &mut self,
        tx: &mut Transaction,
        remaining_gas: u128,
    ) -> Result<TransactionExecutionInfo, StarknetStateError> {
        let tx = tx.execute(&mut self.state, &self.block_context, remaining_gas)?;
        let tx_execution_info = ExecutionInfo::Transaction(Box::new(tx.clone()));
        self.add_messages_and_events(&tx_execution_info)?;
        Ok(tx)
    }

    pub fn add_messages_and_events(
        &mut self,
        exec_info: &ExecutionInfo,
    ) -> Result<(), StarknetStateError> {
        for msg in exec_info.get_sorted_l2_to_l1_messages()? {
            let starknet_message =
                StarknetMessageToL1::new(msg.from_address, msg.to_address, msg.payload);

            self.l2_to_l1_messages_log.push(starknet_message.clone());
            let message_hash = starknet_message.get_hash();

            if self.l2_to_l1_messages.contains_key(&message_hash) {
                let val = self.l2_to_l1_messages.get(&message_hash).unwrap();
                self.l2_to_l1_messages.insert(message_hash, val + 1);
            } else {
                self.l2_to_l1_messages.insert(message_hash, 1);
            }
        }

        let mut events = exec_info.get_sorted_events()?;
        self.events.append(&mut events);
        Ok(())
    }

    /// Consumes the given message hash.
    pub fn consume_message_hash(
        &mut self,
        message_hash: Vec<u8>,
    ) -> Result<(), StarknetStateError> {
        let val = self
            .l2_to_l1_messages
            .get(&message_hash)
            .ok_or(StarknetStateError::InvalidMessageHash)?;

        if val.is_zero() {
            Err(StarknetStateError::InvalidMessageHash)
        } else {
            self.l2_to_l1_messages.insert(message_hash, val - 1);
            Ok(())
        }
    }

    // ------------------------
    //    Private functions
    // ------------------------

    fn chain_id(&self) -> Felt252 {
        self.block_context.starknet_os_config.chain_id.clone()
    }

    #[allow(clippy::too_many_arguments)]
    fn create_invoke_function(
        &mut self,
        contract_address: Address,
        entry_point_selector: Felt252,
        calldata: Vec<Felt252>,
        max_fee: u128,
        signature: Option<Vec<Felt252>>,
        nonce: Option<Felt252>,
        hash_value: Option<Felt252>,
    ) -> Result<InvokeFunction, TransactionError> {
        let signature = match signature {
            Some(sign) => sign,
            None => Vec::new(),
        };

        let nonce = match nonce {
            Some(n) => n,
            None => self.state.get_nonce_at(&contract_address)?,
        };

        match hash_value {
            None => InvokeFunction::new(
                contract_address,
                entry_point_selector,
                max_fee,
                TRANSACTION_VERSION.clone(),
                calldata,
                signature,
                self.chain_id(),
                Some(nonce),
            ),
            Some(hash_value) => InvokeFunction::new_with_tx_hash(
                contract_address,
                entry_point_selector,
                max_fee,
                TRANSACTION_VERSION.clone(),
                calldata,
                signature,
                Some(nonce),
                hash_value,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cairo_vm::vm::runners::cairo_runner::ExecutionResources;
    use num_traits::Num;

    use super::*;
    use crate::{
        core::contract_address::compute_deprecated_class_hash,
        definitions::{
            constants::CONSTRUCTOR_ENTRY_POINT_SELECTOR, transaction_type::TransactionType,
        },
        execution::{CallType, OrderedL2ToL1Message},
        hash_utils::calculate_contract_address,
        state::state_cache::StorageEntry,
        utils::{calculate_sn_keccak, felt_to_hash},
    };

    #[test]
    fn test_deploy() {
        let mut starknet_state = StarknetState::new(None);

        let contract_class = ContractClass::from_path("starknet_programs/fibonacci.json").unwrap();

        let contract_address_salt: Felt252 = 1.into();

        // expected results

        // ----- calculate fib class hash ---------
        let hash = compute_deprecated_class_hash(&contract_class).unwrap();
        let class_hash = felt_to_hash(&hash);

        let address = calculate_contract_address(
            &contract_address_salt,
            &hash,
            &[],
            Address(Felt252::zero()),
        )
        .unwrap();

        let mut actual_resources = HashMap::new();
        actual_resources.insert("l1_gas_usage".to_string(), 3672);
        actual_resources.insert("n_steps".to_string(), 0);

        let transaction_exec_info = TransactionExecutionInfo {
            validate_info: None,
            call_info: Some(CallInfo {
                caller_address: Address(0.into()),
                call_type: Some(CallType::Call),
                contract_address: Address(address.clone()),
                code_address: None,
                class_hash: Some(class_hash),
                entry_point_selector: Some(CONSTRUCTOR_ENTRY_POINT_SELECTOR.clone()),
                entry_point_type: Some(EntryPointType::Constructor),
                ..Default::default()
            }),
            revert_error: None,
            fee_transfer_info: None,
            actual_fee: 0,
            actual_resources,
            tx_type: Some(TransactionType::Deploy),
        };

        // check result is correct
        let exec = (Address(address), transaction_exec_info);
        assert_eq!(
            starknet_state
                .deploy(
                    contract_class.clone(),
                    vec![],
                    contract_address_salt,
                    None,
                    0
                )
                .unwrap(),
            exec
        );

        // check that properly stored contract class
        assert_eq!(
            starknet_state
                .state
                .contract_classes
                .unwrap()
                .get(&class_hash)
                .unwrap()
                .to_owned(),
            contract_class
        );
    }

    #[test]
    fn test_declare() {
        let path = PathBuf::from("starknet_programs/account_without_validation.json");
        let contract_class = ContractClass::from_path(path).unwrap();

        // Instantiate CachedState
        let mut contract_class_cache = HashMap::new();

        //  ------------ contract data --------------------
        // hack store account contract
        let hash = compute_deprecated_class_hash(&contract_class).unwrap();
        let class_hash = felt_to_hash(&hash);
        contract_class_cache.insert(class_hash, contract_class.clone());

        // store sender_address
        let sender_address = Address(1.into());
        // this is not conceptually correct as the sender address would be an
        // Account contract (not the contract that we are currently declaring)
        // but for testing reasons its ok
        let nonce = Felt252::zero();
        let storage_entry: StorageEntry = (sender_address.clone(), [19; 32]);
        let storage = Felt252::zero();

        let mut state_reader = InMemoryStateReader::default();
        state_reader
            .address_to_class_hash_mut()
            .insert(sender_address.clone(), class_hash);
        state_reader
            .address_to_nonce_mut()
            .insert(sender_address.clone(), nonce.clone());
        state_reader
            .address_to_storage_mut()
            .insert(storage_entry.clone(), storage.clone());
        state_reader
            .class_hash_to_contract_class_mut()
            .insert(class_hash, contract_class.clone());

        let state = CachedState::new(Arc::new(state_reader), Some(contract_class_cache), None);

        //* --------------------------------------------
        //*    Create starknet state with previous data
        //* --------------------------------------------

        let mut starknet_state = StarknetState::new(None);

        starknet_state.state = state;
        starknet_state
            .state
            .set_class_hash_at(sender_address.clone(), class_hash)
            .unwrap();

        starknet_state
            .state
            .cache
            .nonce_writes
            .insert(sender_address.clone(), nonce);

        starknet_state.state.set_storage_at(&storage_entry, storage);

        starknet_state
            .state
            .set_contract_class(&class_hash, &contract_class)
            .unwrap();

        // --------------------------------------------
        //      Test declare with starknet state
        // --------------------------------------------
        let fib_contract_class =
            ContractClass::from_path("starknet_programs/fibonacci.json").unwrap();

        let (ret_class_hash, _exec_info) =
            starknet_state.declare(fib_contract_class.clone()).unwrap();

        //* ---------------------------------------
        //              Expected result
        //* ---------------------------------------

        // ----- calculate fib class hash ---------
        let hash = compute_deprecated_class_hash(&fib_contract_class).unwrap();
        let fib_class_hash = felt_to_hash(&hash);

        // check that it return the correct clash hash
        assert_eq!(ret_class_hash, fib_class_hash);

        // check that state has store has store accounts class hash
        assert_eq!(
            starknet_state
                .state
                .get_class_hash_at(&sender_address)
                .unwrap()
                .to_owned(),
            class_hash
        );
        // check that state has store fib class hash
        assert_eq!(
            TryInto::<ContractClass>::try_into(
                starknet_state
                    .state
                    .get_contract_class(&fib_class_hash)
                    .unwrap()
            )
            .unwrap(),
            fib_contract_class
        );
    }

    #[test]
    fn test_invoke() {
        // 1) deploy fibonacci
        // 2) invoke call over fibonacci

        let mut starknet_state = StarknetState::new(None);
        let contract_class = ContractClass::from_path("starknet_programs/fibonacci.json").unwrap();
        let calldata = [1.into(), 1.into(), 10.into()].to_vec();
        let contract_address_salt: Felt252 = 1.into();

        let (contract_address, _exec_info) = starknet_state
            .deploy(
                contract_class.clone(),
                vec![],
                contract_address_salt.clone(),
                None,
                0,
            )
            .unwrap();

        // fibonacci selector
        let selector = Felt252::from_str_radix(
            "112e35f48499939272000bd72eb840e502ca4c3aefa8800992e8defb746e0c9",
            16,
        )
        .unwrap();

        // Statement **not** in blockifier.
        starknet_state
            .state
            .cache_mut()
            .nonce_initial_values_mut()
            .insert(contract_address.clone(), Felt252::zero());

        let tx_info = starknet_state
            .invoke_raw(
                contract_address,
                selector.clone(),
                calldata,
                0,
                Some(Vec::new()),
                Some(Felt252::zero()),
                None,
                0,
            )
            .unwrap();

        // expected result
        // ----- calculate fib class hash ---------
        let hash = compute_deprecated_class_hash(&contract_class).unwrap();
        let fib_class_hash = felt_to_hash(&hash);

        let address = calculate_contract_address(
            &contract_address_salt,
            &hash,
            &[],
            Address(Felt252::zero()),
        )
        .unwrap();
        let actual_resources = HashMap::from([
            ("n_steps".to_string(), 3457),
            ("l1_gas_usage".to_string(), 2448),
            ("range_check_builtin".to_string(), 80),
            ("pedersen_builtin".to_string(), 16),
        ]);

        let expected_info = TransactionExecutionInfo {
            validate_info: None,
            call_info: Some(CallInfo {
                caller_address: Address(Felt252::zero()),
                call_type: Some(CallType::Call),
                contract_address: Address(address),
                code_address: None,
                class_hash: Some(fib_class_hash),
                entry_point_selector: Some(selector),
                entry_point_type: Some(EntryPointType::External),
                calldata: vec![1.into(), 1.into(), 10.into()],
                retdata: vec![144.into()],
                execution_resources: ExecutionResources {
                    n_steps: 94,
                    n_memory_holes: 0,
                    builtin_instance_counter: HashMap::default(),
                },
                ..Default::default()
            }),
            actual_resources,
            tx_type: Some(TransactionType::InvokeFunction),
            ..Default::default()
        };

        assert_eq!(tx_info, expected_info);
    }

    #[test]
    fn test_execute_entry_point_raw() {
        let mut starknet_state = StarknetState::new(None);
        let path = PathBuf::from("starknet_programs/fibonacci.json");
        let contract_class = ContractClass::from_path(path).unwrap();
        let contract_address_salt = 1.into();

        let (contract_address, _exec_info) = starknet_state
            .deploy(contract_class, vec![], contract_address_salt, None, 0)
            .unwrap();

        // fibonacci selector
        let entrypoint_selector = Felt252::from_bytes_be(&calculate_sn_keccak(b"fib"));
        let result = starknet_state
            .execute_entry_point_raw(
                contract_address,
                entrypoint_selector,
                vec![1.into(), 1.into(), 10.into()],
                Address(0.into()),
            )
            .unwrap()
            .retdata;
        assert_eq!(result, vec![144.into()]);
    }

    #[test]
    fn test_add_messages_and_events() {
        let mut starknet_state = StarknetState::new(None);
        let test_msg_1 = OrderedL2ToL1Message {
            order: 0,
            to_address: Address(0.into()),
            payload: vec![0.into()],
        };
        let test_msg_2 = OrderedL2ToL1Message {
            order: 1,
            to_address: Address(0.into()),
            payload: vec![0.into()],
        };

        let exec_info = ExecutionInfo::Call(Box::new(CallInfo {
            l2_to_l1_messages: vec![test_msg_1, test_msg_2],
            ..Default::default()
        }));

        starknet_state.add_messages_and_events(&exec_info).unwrap();
        let msg_hash =
            StarknetMessageToL1::new(Address(0.into()), Address(0.into()), vec![0.into()])
                .get_hash();

        let messages = starknet_state.l2_to_l1_messages;
        let mut expected_messages = HashMap::new();
        expected_messages.insert(msg_hash, 2);
        assert_eq!(messages, expected_messages);
    }

    #[test]
    fn test_consume_message_hash() {
        let mut starknet_state = StarknetState::new(None);
        let test_msg_1 = OrderedL2ToL1Message {
            order: 0,
            to_address: Address(0.into()),
            payload: vec![0.into()],
        };
        let test_msg_2 = OrderedL2ToL1Message {
            order: 1,
            to_address: Address(0.into()),
            payload: vec![0.into()],
        };

        let exec_info = ExecutionInfo::Call(Box::new(CallInfo {
            l2_to_l1_messages: vec![test_msg_1, test_msg_2],
            ..Default::default()
        }));

        starknet_state.add_messages_and_events(&exec_info).unwrap();
        let msg_hash =
            StarknetMessageToL1::new(Address(0.into()), Address(0.into()), vec![0.into()])
                .get_hash();

        starknet_state
            .consume_message_hash(msg_hash.clone())
            .unwrap();
        let messages = starknet_state.l2_to_l1_messages;
        let mut expected_messages = HashMap::new();
        expected_messages.insert(msg_hash, 1);
        assert_eq!(messages, expected_messages);
    }

    #[test]
    fn test_consume_message_hash_twice_should_fail() {
        let mut starknet_state = StarknetState::new(None);
        let test_msg = OrderedL2ToL1Message {
            order: 0,
            to_address: Address(0.into()),
            payload: vec![0.into()],
        };

        let exec_info = ExecutionInfo::Call(Box::new(CallInfo {
            l2_to_l1_messages: vec![test_msg],
            ..Default::default()
        }));

        starknet_state.add_messages_and_events(&exec_info).unwrap();
        let msg_hash =
            StarknetMessageToL1::new(Address(0.into()), Address(0.into()), vec![0.into()])
                .get_hash();

        starknet_state
            .consume_message_hash(msg_hash.clone())
            .unwrap();
        let err = starknet_state.consume_message_hash(msg_hash).unwrap_err();
        assert_matches!(err, StarknetStateError::InvalidMessageHash);
    }
}
