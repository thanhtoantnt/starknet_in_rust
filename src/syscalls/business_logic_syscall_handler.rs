#![allow(clippy::absurd_extreme_comparisons)]

use std::collections::HashMap;
use std::ops::Add;

use super::syscall_handler_errors::SyscallHandlerError;
use super::syscall_request::{
    EmitEventRequest, FromPtr, GetBlockHashRequest, GetBlockTimestampRequest, KeccakRequest,
    StorageReadRequest, StorageWriteRequest,
};
use super::syscall_response::{
    DeployResponse, GetBlockHashResponse, GetBlockTimestampResponse, KeccakResponse,
    SyscallResponse,
};
use super::{
    syscall_info::get_syscall_size_from_name,
    syscall_request::{
        CallContractRequest, DeployRequest, LibraryCallRequest, ReplaceClassRequest,
        SendMessageToL1Request, SyscallRequest,
    },
    syscall_response::{CallContractResponse, FailureReason, ResponseBody},
};
use crate::definitions::block_context::BlockContext;
use crate::definitions::constants::BLOCK_HASH_CONTRACT_ADDRESS;
use crate::execution::execution_entry_point::ExecutionResult;
use crate::services::api::contract_classes::compiled_class::CompiledClass;
use crate::state::cached_state::CachedState;
use crate::state::BlockInfo;
use crate::transaction::error::TransactionError;
use crate::utils::calculate_sn_keccak;
use crate::{
    core::errors::state_errors::StateError,
    definitions::constants::CONSTRUCTOR_ENTRY_POINT_SELECTOR,
    execution::{
        execution_entry_point::ExecutionEntryPoint, CallInfo, CallResult, CallType, OrderedEvent,
        OrderedL2ToL1Message, TransactionExecutionContext,
    },
    hash_utils::calculate_contract_address,
    services::api::contract_class_errors::ContractClassError,
    state::ExecutionResourcesManager,
    state::{
        contract_storage_state::ContractStorageState,
        state_api::{State, StateReader},
    },
    utils::{felt_to_hash, get_big_int, get_felt_range, Address, ClassHash},
};
use cairo_vm::felt::Felt252;
use cairo_vm::{
    types::{
        errors::math_errors::MathError,
        relocatable::{MaybeRelocatable, Relocatable},
    },
    vm::{errors::memory_errors::MemoryError, vm_core::VirtualMachine},
};
use lazy_static::lazy_static;

use crate::services::api::contract_classes::deprecated_contract_class::EntryPointType;
use num_traits::{One, ToPrimitive, Zero};

const STEP: u128 = 100;
const SYSCALL_BASE: u128 = 100 * STEP;
const KECCAK_ROUND_COST: u128 = 180000;
lazy_static! {
    /// Felt->syscall map that was extracted from new_syscalls.json (Cairo 1.0 syscalls)
    static ref SELECTOR_TO_SYSCALL: HashMap<Felt252, &'static str> = {
            let mut map: HashMap<Felt252, &'static str> = HashMap::with_capacity(9);

            map.insert(92376026794327011772951660_u128.into(), "library_call");
            map.insert(25500403217443378527601783667_u128.into(), "replace_class");
            map.insert(
                94901967946959054011942058057773508207_u128.into(),
                "get_execution_info",
            );
            map.insert(100890693370601760042082660_u128.into(), "storage_read");
            map.insert(20853273475220472486191784820_u128.into(), "call_contract");
            map.insert(
                433017908768303439907196859243777073_u128.into(),
                "send_message_to_l1",
            );
            map.insert(75202468540281_u128.into(), "deploy");
            map.insert(1280709301550335749748_u128.into(), "emit_event");
            map.insert(25828017502874050592466629733_u128.into(), "storage_write");
            map.insert(Felt252::from_bytes_be(&calculate_sn_keccak("get_block_timestamp".as_bytes())), "get_block_timestamp");
            map.insert(Felt252::from_bytes_be(&calculate_sn_keccak("get_block_number".as_bytes())), "get_block_number");
            map.insert(Felt252::from_bytes_be("Keccak".as_bytes()), "keccak");

            map
    };

    // TODO: There is no reason why this could not be in the syscall enum itself AFAICT
    // Taken from starkware/starknet/constants.py in cairo-lang
    // See further documentation on cairo_programs/constants.cairo
    /// Maps syscall name to gas costs
    static ref SYSCALL_GAS_COST: HashMap<&'static str, u128> = {
        let mut map = HashMap::new();

        map.insert("initial", 100_000_000 * STEP);
        map.insert("entry_point_initial_budget", 100 * STEP);

        map.insert("entry_point", map["entry_point_initial_budget"] + 500 * STEP);
        map.insert("fee_transfer", map["entry_point"] + 100 * STEP);
        map.insert("transaction", 2 * map["entry_point"] + map["fee_transfer"] + 100 * STEP);

        map.insert("call_contract", SYSCALL_BASE + 10 * STEP + map["entry_point"]);
        map.insert("deploy", SYSCALL_BASE + 200 * STEP + map["entry_point"]);
        map.insert("get_execution_info", SYSCALL_BASE + 10 * STEP);
        map.insert("library_call", map["call_contract"]);
        map.insert("replace_class", SYSCALL_BASE + 50 * STEP);
        map.insert("storage_read", SYSCALL_BASE + 50 * STEP);
        map.insert("storage_write", SYSCALL_BASE + 50 * STEP);
        map.insert("emit_event", SYSCALL_BASE + 10 * STEP);
        map.insert("send_message_to_l1", SYSCALL_BASE + 50 * STEP);
        map.insert("get_block_timestamp", 0);
        map.insert("keccak", 0);

        map
    };
}

#[derive(Debug)]
pub struct BusinessLogicSyscallHandler<'a, S: StateReader> {
    pub(crate) events: Vec<OrderedEvent>,
    pub(crate) expected_syscall_ptr: Relocatable,
    pub(crate) resources_manager: ExecutionResourcesManager,
    pub(crate) tx_execution_context: TransactionExecutionContext,
    pub(crate) l2_to_l1_messages: Vec<OrderedL2ToL1Message>,
    pub(crate) contract_address: Address,
    pub(crate) caller_address: Address,
    pub(crate) read_only_segments: Vec<(Relocatable, MaybeRelocatable)>,
    pub(crate) internal_calls: Vec<CallInfo>,
    pub(crate) block_context: BlockContext,
    pub(crate) starknet_storage_state: ContractStorageState<'a, S>,
    pub(crate) support_reverted: bool,
    pub(crate) entry_point_selector: Felt252,
    pub(crate) selector_to_syscall: &'a HashMap<Felt252, &'static str>,
}

// TODO: execution entry point may no be a parameter field, but there is no way to generate a default for now

impl<'a, S: StateReader> BusinessLogicSyscallHandler<'a, S> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        tx_execution_context: TransactionExecutionContext,
        state: &'a mut CachedState<S>,
        resources_manager: ExecutionResourcesManager,
        caller_address: Address,
        contract_address: Address,
        block_context: BlockContext,
        syscall_ptr: Relocatable,
        support_reverted: bool,
        entry_point_selector: Felt252,
    ) -> Self {
        let events = Vec::new();
        let read_only_segments = Vec::new();
        let l2_to_l1_messages = Vec::new();
        let starknet_storage_state = ContractStorageState::new(state, contract_address.clone());
        let internal_calls = Vec::new();

        BusinessLogicSyscallHandler {
            tx_execution_context,
            events,
            read_only_segments,
            resources_manager,
            contract_address,
            caller_address,
            l2_to_l1_messages,
            block_context,
            starknet_storage_state,
            internal_calls,
            expected_syscall_ptr: syscall_ptr,
            support_reverted,
            entry_point_selector,
            selector_to_syscall: &SELECTOR_TO_SYSCALL,
        }
    }
    pub fn default_with_state(state: &'a mut CachedState<S>) -> Self {
        BusinessLogicSyscallHandler::new_for_testing(
            BlockInfo::default(),
            Default::default(),
            state,
        )
    }

    pub fn new_for_testing(
        block_info: BlockInfo,
        _contract_address: Address,
        state: &'a mut CachedState<S>,
    ) -> Self {
        let syscalls = Vec::from([
            "emit_event".to_string(),
            "deploy".to_string(),
            "get_tx_info".to_string(),
            "send_message_to_l1".to_string(),
            "library_call".to_string(),
            "get_caller_address".to_string(),
            "get_contract_address".to_string(),
            "get_sequencer_address".to_string(),
            "get_block_timestamp".to_string(),
        ]);
        let events = Vec::new();
        let tx_execution_context = Default::default();
        let read_only_segments = Vec::new();
        let resources_manager = ExecutionResourcesManager::new(syscalls, Default::default());
        let contract_address = Address(1.into());
        let caller_address = Address(0.into());
        let l2_to_l1_messages = Vec::new();
        let mut block_context = BlockContext::default();
        block_context.block_info = block_info;
        let starknet_storage_state = ContractStorageState::new(state, contract_address.clone());

        let internal_calls = Vec::new();
        let expected_syscall_ptr = Relocatable::from((0, 0));
        let entry_point_selector = 333.into();

        BusinessLogicSyscallHandler {
            tx_execution_context,
            events,
            read_only_segments,
            resources_manager,
            contract_address,
            caller_address,
            l2_to_l1_messages,
            block_context,
            starknet_storage_state,
            internal_calls,
            expected_syscall_ptr,
            support_reverted: false,
            entry_point_selector,
            selector_to_syscall: &SELECTOR_TO_SYSCALL,
        }
    }

    /// Increments the syscall count for a given `syscall_name` by 1.
    fn increment_syscall_count(&mut self, syscall_name: &str) {
        self.resources_manager
            .increment_syscall_counter(syscall_name, 1);
    }

    fn call_contract_helper(
        &mut self,
        vm: &mut VirtualMachine,
        remaining_gas: u128,
        execution_entry_point: ExecutionEntryPoint,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let ExecutionResult {
            call_info,
            revert_error,
            ..
        } = execution_entry_point
            .execute(
                self.starknet_storage_state.state,
                &self.block_context,
                &mut self.resources_manager,
                &mut self.tx_execution_context,
                false,
                self.block_context.invoke_tx_max_n_steps,
                false,
            )
            .map_err(|err| SyscallHandlerError::ExecutionError(err.to_string()))?;

        let call_info = call_info.ok_or(SyscallHandlerError::ExecutionError(
            revert_error.unwrap_or("Execution error".to_string()),
        ))?;

        let retdata_maybe_reloc = call_info
            .retdata
            .clone()
            .into_iter()
            .map(|item| MaybeRelocatable::from(Felt252::new(item)))
            .collect::<Vec<MaybeRelocatable>>();

        let retdata_start = self.allocate_segment(vm, retdata_maybe_reloc)?;
        let retdata_end = (retdata_start + call_info.retdata.len())?;

        let remaining_gas = remaining_gas.saturating_sub(call_info.gas_consumed);

        let gas = remaining_gas;
        let body = if call_info.failure_flag {
            Some(ResponseBody::Failure(FailureReason {
                retdata_start,
                retdata_end,
            }))
        } else {
            Some(ResponseBody::CallContract(CallContractResponse {
                retdata_start,
                retdata_end,
            }))
        };

        // update syscall handler information
        self.starknet_storage_state
            .read_values
            .extend(call_info.storage_read_values.clone());
        self.starknet_storage_state
            .accessed_keys
            .extend(call_info.accessed_storage_keys.clone());

        self.internal_calls.push(call_info);

        Ok(SyscallResponse { gas, body })
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

    fn execute_constructor_entry_point(
        &mut self,
        contract_address: &Address,
        class_hash_bytes: ClassHash,
        constructor_calldata: Vec<Felt252>,
        remainig_gas: u128,
    ) -> Result<CallResult, StateError> {
        let compiled_class = if let Ok(compiled_class) = self
            .starknet_storage_state
            .state
            .get_contract_class(&class_hash_bytes)
        {
            compiled_class
        } else {
            return Ok(CallResult {
                gas_consumed: 0,
                is_success: false,
                retdata: vec![Felt252::from_bytes_be(b"CLASS_HASH_NOT_FOUND").into()],
            });
        };

        if self.constructor_entry_points_empty(compiled_class)? {
            if !constructor_calldata.is_empty() {
                return Err(StateError::ConstructorCalldataEmpty());
            }

            let call_info = CallInfo::empty_constructor_call(
                contract_address.clone(),
                self.contract_address.clone(),
                Some(class_hash_bytes),
            );
            self.internal_calls.push(call_info.clone());

            return Ok(call_info.result());
        }

        let call = ExecutionEntryPoint::new(
            contract_address.clone(),
            constructor_calldata,
            CONSTRUCTOR_ENTRY_POINT_SELECTOR.clone(),
            self.contract_address.clone(),
            EntryPointType::Constructor,
            Some(CallType::Call),
            None,
            remainig_gas,
        );

        let ExecutionResult {
            call_info,
            revert_error,
            ..
        } = call
            .execute(
                self.starknet_storage_state.state,
                &self.block_context,
                &mut self.resources_manager,
                &mut self.tx_execution_context,
                self.support_reverted,
                self.block_context.invoke_tx_max_n_steps,
                false,
            )
            .map_err(|_| StateError::ExecutionEntryPoint())?;

        let call_info = call_info.ok_or(StateError::CustomError(
            revert_error.unwrap_or("Execution error".to_string()),
        ))?;

        self.internal_calls.push(call_info.clone());

        Ok(call_info.result())
    }

    fn syscall_storage_write(&mut self, key: Felt252, value: Felt252) {
        self.starknet_storage_state.write(&key.to_be_bytes(), value)
    }

    pub fn syscall(
        &mut self,
        vm: &mut VirtualMachine,
        syscall_ptr: Relocatable,
    ) -> Result<(), SyscallHandlerError> {
        let selector = get_big_int(vm, syscall_ptr)?;
        let syscall_name = self.selector_to_syscall.get(&selector).ok_or(
            SyscallHandlerError::SelectorNotInHandlerMap(selector.to_string()),
        )?;

        let initial_gas: Felt252 = get_big_int(vm, (syscall_ptr + 1)?)?;
        let initial_gas = initial_gas
            .to_u128()
            .ok_or(MathError::Felt252ToU64Conversion(Box::new(initial_gas)))?;

        // Advance SyscallPointer as the first two cells contain the selector & gas
        let mut syscall_ptr: Relocatable =
            (syscall_ptr + 2_usize).map_err(SyscallHandlerError::from)?;

        let request = self.read_and_validate_syscall_request(vm, &mut syscall_ptr, syscall_name)?;

        // Check and reduce gas (after validating the syscall selector for consistency wth the OS).
        let required_gas = SYSCALL_GAS_COST
            .get(syscall_name)
            .map(|&x| x.saturating_sub(SYSCALL_BASE))
            .ok_or(SyscallHandlerError::SelectorDoesNotHaveAssociatedGas(
                selector.to_string(),
            ))?;

        let response = if initial_gas < required_gas {
            let out_of_gas_felt = Felt252::from_bytes_be("Out of gas".as_bytes());
            let retdata_start =
                self.allocate_segment(vm, vec![MaybeRelocatable::from(out_of_gas_felt)])?;
            let response_body = ResponseBody::Failure(FailureReason {
                retdata_start,
                retdata_end: (retdata_start + 1)?,
            });

            SyscallResponse {
                gas: initial_gas,
                body: Some(response_body),
            }
        } else {
            // Execute with remaining gas.
            let remaining_gas = initial_gas - required_gas;
            self.execute_syscall(request, remaining_gas, vm)?
        };

        // Write response to the syscall segment.
        self.expected_syscall_ptr = vm
            .write_arg(syscall_ptr, &response.to_cairo_compatible_args())?
            .get_relocatable()
            .ok_or(MemoryError::WriteArg)?;

        Ok(())
    }

    fn execute_syscall(
        &mut self,
        request: SyscallRequest,
        remaining_gas: u128,
        vm: &mut VirtualMachine,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        match request {
            SyscallRequest::LibraryCall(req) => self.library_call(vm, req, remaining_gas),
            SyscallRequest::CallContract(req) => self.call_contract(vm, req, remaining_gas),
            SyscallRequest::Deploy(req) => self.deploy(vm, req, remaining_gas),
            SyscallRequest::StorageRead(req) => self.storage_read(vm, req, remaining_gas),
            SyscallRequest::StorageWrite(req) => self.storage_write(vm, req, remaining_gas),
            SyscallRequest::GetExecutionInfo => self.get_execution_info(vm, remaining_gas),
            SyscallRequest::SendMessageToL1(req) => self.send_message_to_l1(vm, req, remaining_gas),
            SyscallRequest::EmitEvent(req) => self.emit_event(vm, req, remaining_gas),
            SyscallRequest::GetBlockNumber => self.get_block_number(vm, remaining_gas),
            SyscallRequest::GetBlockTimestamp(req) => {
                self.get_block_timestamp(vm, req, remaining_gas)
            }
            SyscallRequest::GetBlockHash(req) => self.get_block_hash(vm, req, remaining_gas),
            SyscallRequest::ReplaceClass(req) => self.replace_class(vm, req, remaining_gas),
            SyscallRequest::Keccak(req) => self.keccak(vm, req, remaining_gas),
        }
    }

    fn get_block_hash(
        &mut self,
        vm: &mut VirtualMachine,
        request: GetBlockHashRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let block_number = request.block_number;
        let current_block_number = self.block_context.block_info.block_number;

        if block_number > current_block_number - 10 {
            let out_of_range_felt = Felt252::from_bytes_be("Block number out of range".as_bytes());
            let retdata_start =
                self.allocate_segment(vm, vec![MaybeRelocatable::from(out_of_range_felt)])?;
            let failure = FailureReason {
                retdata_start,
                retdata_end: (retdata_start + 1)?,
            };

            return Ok(SyscallResponse {
                gas: remaining_gas,
                body: Some(ResponseBody::Failure(failure)),
            });
        }

        // FIXME: Update this after release.
        const V_0_12_0_FIRST_BLOCK: u64 = 0;
        let block_hash = if block_number < V_0_12_0_FIRST_BLOCK {
            Felt252::zero()
        } else {
            self.starknet_storage_state.state.get_storage_at(&(
                BLOCK_HASH_CONTRACT_ADDRESS.clone(),
                Felt252::new(block_number).to_be_bytes(),
            ))?
        };

        Ok(SyscallResponse {
            gas: remaining_gas,
            body: Some(ResponseBody::GetBlockHash(GetBlockHashResponse {
                block_hash,
            })),
        })
    }

    pub(crate) fn post_run(
        &self,
        runner: &mut VirtualMachine,
        syscall_stop_ptr: Relocatable,
    ) -> Result<(), TransactionError> {
        let expected_stop_ptr = self.expected_syscall_ptr;
        if syscall_stop_ptr != expected_stop_ptr {
            return Err(TransactionError::InvalidStopPointer(
                expected_stop_ptr,
                syscall_stop_ptr,
            ));
        }
        self.validate_read_only_segments(runner)
    }

    /// Validates that there were no out of bounds writes to read-only segments and marks
    /// them as accessed.
    pub(crate) fn validate_read_only_segments(
        &self,
        runner: &mut VirtualMachine,
    ) -> Result<(), TransactionError> {
        for (segment_ptr, segment_size) in self.read_only_segments.clone() {
            let used_size = runner
                .get_segment_used_size(segment_ptr.segment_index as usize)
                .ok_or(TransactionError::InvalidSegmentSize)?;

            let seg_size = match segment_size {
                MaybeRelocatable::Int(size) => size,
                _ => return Err(TransactionError::NotAFelt),
            };

            if seg_size != used_size.into() {
                return Err(TransactionError::OutOfBound);
            }
            runner.mark_address_range_as_accessed(segment_ptr, used_size)?;
        }
        Ok(())
    }
}

impl<'a, S: StateReader> BusinessLogicSyscallHandler<'a, S> {
    fn emit_event(
        &mut self,
        vm: &VirtualMachine,
        request: EmitEventRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let order = self.tx_execution_context.n_emitted_events;
        let keys: Vec<Felt252> = get_felt_range(vm, request.keys_start, request.keys_end)?;
        let data: Vec<Felt252> = get_felt_range(vm, request.data_start, request.data_end)?;
        self.events.push(OrderedEvent::new(order, keys, data));

        // Update events count.
        self.tx_execution_context.n_emitted_events += 1;
        Ok(SyscallResponse {
            gas: remaining_gas,
            body: None,
        })
    }

    fn get_block_number(
        &mut self,
        _vm: &mut VirtualMachine,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        Ok(SyscallResponse {
            gas: remaining_gas,
            body: Some(ResponseBody::GetBlockNumber {
                number: self.block_context.block_info.block_number.into(),
            }),
        })
    }

    fn _storage_read(&mut self, key: [u8; 32]) -> Result<Felt252, StateError> {
        match self.starknet_storage_state.read(&key) {
            Ok(value) => Ok(value),
            Err(e @ StateError::Io(_)) => Err(e),
            Err(_) => Ok(Felt252::zero()),
        }
    }

    fn storage_write(
        &mut self,
        vm: &mut VirtualMachine,
        request: StorageWriteRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        if request.reserved != 0.into() {
            let retdata_start = self.allocate_segment(
                vm,
                vec![Felt252::from_bytes_be(b"Unsupported address domain").into()],
            )?;
            let retdata_end = retdata_start.add(1)?;

            return Ok(SyscallResponse {
                gas: remaining_gas,
                body: Some(ResponseBody::Failure(FailureReason {
                    retdata_start,
                    retdata_end,
                })),
            });
        }

        self.syscall_storage_write(request.key, request.value);

        Ok(SyscallResponse {
            gas: remaining_gas,
            body: None,
        })
    }

    fn get_execution_info(
        &self,
        vm: &mut VirtualMachine,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let tx_info = &self.tx_execution_context;
        let block_info = &self.block_context.block_info;

        let mut res_segment = vm.add_memory_segment();

        let signature_start = res_segment;
        for s in tx_info.signature.iter() {
            vm.insert_value(res_segment, s)?;
            res_segment = (res_segment + 1)?;
        }
        let signature_end = res_segment;

        let tx_info_ptr = res_segment;
        vm.insert_value::<Felt252>(res_segment, tx_info.version.clone())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value(res_segment, tx_info.account_contract_address.0.clone())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, tx_info.max_fee.into())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value(res_segment, signature_start)?;
        res_segment = (res_segment + 1)?;
        vm.insert_value(res_segment, signature_end)?;
        res_segment = (res_segment + 1)?;
        vm.insert_value(res_segment, tx_info.transaction_hash.clone())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(
            res_segment,
            self.block_context.starknet_os_config.chain_id.clone(),
        )?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, tx_info.nonce.clone())?;
        res_segment = (res_segment + 1)?;

        let block_info_ptr = res_segment;
        vm.insert_value::<Felt252>(res_segment, block_info.block_number.into())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, block_info.block_timestamp.into())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, block_info.sequencer_address.0.clone())?;
        res_segment = (res_segment + 1)?;

        let exec_info_ptr = res_segment;
        vm.insert_value(res_segment, block_info_ptr)?;
        res_segment = (res_segment + 1)?;
        vm.insert_value(res_segment, tx_info_ptr)?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, self.caller_address.0.clone())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, self.contract_address.0.clone())?;
        res_segment = (res_segment + 1)?;
        vm.insert_value::<Felt252>(res_segment, self.entry_point_selector.clone())?;

        Ok(SyscallResponse {
            gas: remaining_gas,
            body: Some(ResponseBody::GetExecutionInfo { exec_info_ptr }),
        })
    }

    fn call_contract(
        &mut self,
        vm: &mut VirtualMachine,
        request: CallContractRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let calldata = get_felt_range(vm, request.calldata_start, request.calldata_end)?;
        let execution_entry_point = ExecutionEntryPoint::new(
            request.contract_address,
            calldata,
            request.selector,
            self.contract_address.clone(),
            EntryPointType::External,
            Some(CallType::Call),
            None,
            remaining_gas,
        );

        self.call_contract_helper(vm, remaining_gas, execution_entry_point)
    }

    fn storage_read(
        &mut self,
        vm: &mut VirtualMachine,
        request: StorageReadRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        if request.reserved != Felt252::zero() {
            let retdata_start = self.allocate_segment(
                vm,
                vec![Felt252::from_bytes_be(b"Unsupported address domain").into()],
            )?;
            let retdata_end = retdata_start.add(1)?;

            return Ok(SyscallResponse {
                gas: remaining_gas,
                body: Some(ResponseBody::Failure(FailureReason {
                    retdata_start,
                    retdata_end,
                })),
            });
        }

        let value = self._storage_read(request.key)?;

        Ok(SyscallResponse {
            gas: remaining_gas,
            body: Some(ResponseBody::StorageReadResponse { value: Some(value) }),
        })
    }

    fn syscall_deploy(
        &mut self,
        vm: &VirtualMachine,
        request: DeployRequest,
        remaining_gas: u128,
    ) -> Result<(Address, CallResult), SyscallHandlerError> {
        if !(request.deploy_from_zero.is_zero() || request.deploy_from_zero.is_one()) {
            return Err(SyscallHandlerError::DeployFromZero(
                request.deploy_from_zero,
            ));
        };

        let constructor_calldata =
            get_felt_range(vm, request.calldata_start, request.calldata_end)?;

        let class_hash = &request.class_hash;

        let deployer_address = if request.deploy_from_zero.is_zero() {
            self.contract_address.clone()
        } else {
            Address::default()
        };

        let contract_address = Address(calculate_contract_address(
            &request.salt,
            class_hash,
            &constructor_calldata,
            deployer_address,
        )?);

        // Initialize the contract.
        let class_hash_bytes: ClassHash = felt_to_hash(&request.class_hash);

        if (self
            .starknet_storage_state
            .state
            .deploy_contract(contract_address.clone(), class_hash_bytes))
        .is_err()
        {
            return Ok((
                Address::default(),
                (CallResult {
                    gas_consumed: 0,
                    is_success: false,
                    retdata: vec![Felt252::from_bytes_be(b"CONTRACT_ADDRESS_UNAVAILABLE").into()],
                }),
            ));
        }
        let result = self.execute_constructor_entry_point(
            &contract_address,
            class_hash_bytes,
            constructor_calldata,
            remaining_gas,
        )?;

        Ok((contract_address, result))
    }

    fn deploy(
        &mut self,
        vm: &mut VirtualMachine,
        syscall_request: DeployRequest,
        mut remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let (contract_address, result) = self.syscall_deploy(vm, syscall_request, remaining_gas)?;

        remaining_gas -= result.gas_consumed;

        let retdata_len = result.retdata.len();

        let retdata_start = self.allocate_segment(vm, result.retdata)?;
        let retdata_end = retdata_start.add(retdata_len)?;

        let ok = result.is_success;

        let body: ResponseBody = if ok {
            let contract_address = contract_address.0;
            ResponseBody::Deploy(DeployResponse {
                contract_address,
                retdata_start,
                retdata_end,
            })
        } else {
            ResponseBody::Failure(FailureReason {
                retdata_start,
                retdata_end,
            })
        };
        let response = SyscallResponse {
            gas: remaining_gas,
            body: Some(body),
        };

        Ok(response)
    }

    fn read_syscall_request(
        &self,
        vm: &VirtualMachine,
        syscall_ptr: Relocatable,
        syscall_name: &str,
    ) -> Result<SyscallRequest, SyscallHandlerError> {
        match syscall_name {
            "emit_event" => EmitEventRequest::from_ptr(vm, syscall_ptr),
            "storage_read" => StorageReadRequest::from_ptr(vm, syscall_ptr),
            "call_contract" => CallContractRequest::from_ptr(vm, syscall_ptr),
            "library_call" => LibraryCallRequest::from_ptr(vm, syscall_ptr),
            "deploy" => DeployRequest::from_ptr(vm, syscall_ptr),
            "get_block_number" => Ok(SyscallRequest::GetBlockNumber),
            "storage_write" => StorageWriteRequest::from_ptr(vm, syscall_ptr),
            "get_execution_info" => Ok(SyscallRequest::GetExecutionInfo),
            "send_message_to_l1" => SendMessageToL1Request::from_ptr(vm, syscall_ptr),
            "replace_class" => ReplaceClassRequest::from_ptr(vm, syscall_ptr),
            "keccak" => KeccakRequest::from_ptr(vm, syscall_ptr),
            _ => Err(SyscallHandlerError::UnknownSyscall(
                syscall_name.to_string(),
            )),
        }
    }

    pub(crate) fn allocate_segment(
        &mut self,
        vm: &mut VirtualMachine,
        data: Vec<MaybeRelocatable>,
    ) -> Result<Relocatable, SyscallHandlerError> {
        let segment_start = vm.add_memory_segment();
        let segment_end = vm.write_arg(segment_start, &data)?;
        let sub = segment_end.sub(&segment_start.to_owned().into())?;
        let segment = (segment_start.to_owned(), sub);
        self.read_only_segments.push(segment);

        Ok(segment_start)
    }

    fn send_message_to_l1(
        &mut self,
        vm: &mut VirtualMachine,
        request: SendMessageToL1Request,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let payload = get_felt_range(vm, request.payload_start, request.payload_end)?;

        self.l2_to_l1_messages.push(OrderedL2ToL1Message::new(
            self.tx_execution_context.n_sent_messages,
            request.to_address,
            payload,
        ));

        // Update messages count.
        self.tx_execution_context.n_sent_messages += 1;
        Ok(SyscallResponse {
            gas: remaining_gas,
            body: None,
        })
    }

    fn read_and_validate_syscall_request(
        &mut self,
        vm: &VirtualMachine,
        syscall_ptr: &mut Relocatable,
        syscall_name: &str,
    ) -> Result<SyscallRequest, SyscallHandlerError> {
        self.increment_syscall_count(syscall_name);
        let syscall_request = self.read_syscall_request(vm, *syscall_ptr, syscall_name)?;

        *syscall_ptr += get_syscall_size_from_name(syscall_name);
        Ok(syscall_request)
    }

    fn library_call(
        &mut self,
        vm: &mut VirtualMachine,
        request: LibraryCallRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let calldata = get_felt_range(vm, request.calldata_start, request.calldata_end)?;
        let execution_entry_point = ExecutionEntryPoint::new(
            self.contract_address.clone(),
            calldata,
            request.selector,
            self.caller_address.clone(),
            EntryPointType::External,
            Some(CallType::Delegate),
            Some(request.class_hash.to_be_bytes()),
            remaining_gas,
        );

        self.call_contract_helper(vm, remaining_gas, execution_entry_point)
    }

    fn get_block_timestamp(
        &mut self,
        _vm: &VirtualMachine,
        _request: GetBlockTimestampRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        Ok(SyscallResponse {
            gas: remaining_gas,
            body: Some(ResponseBody::GetBlockTimestamp(GetBlockTimestampResponse {
                timestamp: self.block_context.block_info.block_timestamp.into(),
            })),
        })
    }

    fn replace_class(
        &mut self,
        _vm: &VirtualMachine,
        request: ReplaceClassRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        self.starknet_storage_state.state.set_class_hash_at(
            self.contract_address.clone(),
            request.class_hash.to_be_bytes(),
        )?;
        Ok(SyscallResponse {
            gas: remaining_gas,
            body: None,
        })
    }

    fn keccak(
        &mut self,
        vm: &mut VirtualMachine,
        request: KeccakRequest,
        remaining_gas: u128,
    ) -> Result<SyscallResponse, SyscallHandlerError> {
        let length = (request.input_end - request.input_start)?;
        let mut gas = remaining_gas;

        if length % 17 != 0 {
            let response = self.failure_from_error_msg(vm, b"Invalid keccak input size")?;
            return Ok(SyscallResponse {
                gas,
                body: Some(response),
            });
        }
        let n_chunks = length / 17;
        let mut state = [0u64; 25];
        for i in 0..n_chunks {
            // TODO: check this before the loop, taking care to preserve functionality.
            if gas < KECCAK_ROUND_COST {
                let response = self.failure_from_error_msg(vm, b"Syscall out of gas")?;
                return Ok(SyscallResponse {
                    gas,
                    body: Some(response),
                });
            }
            gas -= KECCAK_ROUND_COST;
            let chunk_start = (request.input_start + i * 17)?;
            let chunk = get_felt_range(vm, chunk_start, (chunk_start + 17)?)?;
            for (i, val) in chunk.iter().enumerate() {
                state[i] ^= val.to_u64().ok_or_else(|| {
                    SyscallHandlerError::Conversion("Felt252".to_string(), "u64".to_string())
                })?;
            }
            keccak::f1600(&mut state)
        }
        let hash_low = (Felt252::from(state[1]) << 64u32) + Felt252::from(state[0]);
        let hash_high = (Felt252::from(state[3]) << 64u32) + Felt252::from(state[2]);
        Ok(SyscallResponse {
            gas,
            body: Some(ResponseBody::Keccak(KeccakResponse {
                hash_low,
                hash_high,
            })),
        })
    }

    // TODO: refactor code to use this function
    fn failure_from_error_msg(
        &mut self,
        vm: &mut VirtualMachine,
        error_msg: &[u8],
    ) -> Result<ResponseBody, SyscallHandlerError> {
        let felt_encoded_msg = Felt252::from_bytes_be(error_msg);
        let retdata_start =
            self.allocate_segment(vm, vec![MaybeRelocatable::from(felt_encoded_msg)])?;
        Ok(ResponseBody::Failure(FailureReason {
            retdata_start,
            retdata_end: (retdata_start + 1)?,
        }))
    }
}
