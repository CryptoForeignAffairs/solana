#![allow(clippy::integer_arithmetic)]
pub mod alloc;
pub mod allocator_bump;
pub mod deprecated;
pub mod serialization;
pub mod syscalls;
pub mod upgradeable;
pub mod upgradeable_with_jit;
pub mod with_jit;

use crate::{
    serialization::{deserialize_parameters, serialize_parameters},
    syscalls::SyscallError,
};
use log::{log_enabled, trace, Level::Trace};
use solana_measure::measure::Measure;
use solana_program_runtime::InstructionProcessor;
use solana_rbpf::{
    aligned_memory::AlignedMemory,
    ebpf::HOST_ALIGN,
    error::{EbpfError, UserDefinedError},
    static_analysis::Analysis,
    verifier::{self, VerifierError},
    vm::{Config, EbpfVm, Executable, InstructionMeter},
};
use solana_sdk::{
    account::{ReadableAccount, WritableAccount},
    account_utils::State,
    bpf_loader, bpf_loader_deprecated,
    bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    clock::Clock,
    entrypoint::{HEAP_LENGTH, SUCCESS},
    feature_set::{
        add_missing_program_error_mappings, close_upgradeable_program_accounts, do_support_realloc,
        fix_write_privs, reduce_required_deploy_balance, stop_verify_mul64_imm_nonzero,
    },
    ic_logger_msg, ic_msg,
    instruction::{AccountMeta, InstructionError},
    keyed_account::{from_keyed_account, keyed_account_at_index, KeyedAccount},
    loader_instruction::LoaderInstruction,
    loader_upgradeable_instruction::UpgradeableLoaderInstruction,
    process_instruction::{stable_log, ComputeMeter, Executor, InvokeContext, Logger},
    program_error::{ACCOUNT_NOT_RENT_EXEMPT, BORSH_IO_ERROR},
    program_utils::limited_deserialize,
    pubkey::Pubkey,
    rent::Rent,
    system_instruction::{self, MAX_PERMITTED_DATA_LENGTH},
};
use std::{cell::RefCell, fmt::Debug, rc::Rc, sync::Arc};
use thiserror::Error;

solana_sdk::declare_builtin!(
    solana_sdk::bpf_loader::ID,
    solana_bpf_loader_program,
    solana_bpf_loader_program::process_instruction
);

/// Errors returned by functions the BPF Loader registers with the VM
#[derive(Debug, Error, PartialEq)]
pub enum BpfError {
    #[error("{0}")]
    VerifierError(#[from] VerifierError),
    #[error("{0}")]
    SyscallError(#[from] SyscallError),
}
impl UserDefinedError for BpfError {}

fn map_ebpf_error(invoke_context: &dyn InvokeContext, e: EbpfError<BpfError>) -> InstructionError {
    ic_msg!(invoke_context, "{}", e);
    InstructionError::InvalidAccountData
}

pub fn create_executor(
    programdata_account_index: usize,
    programdata_offset: usize,
    invoke_context: &mut dyn InvokeContext,
    use_jit: bool,
) -> Result<Arc<BpfExecutor>, InstructionError> {
    let syscall_registry = syscalls::register_syscalls(invoke_context).map_err(|e| {
        ic_msg!(invoke_context, "Failed to register syscalls: {}", e);
        InstructionError::ProgramEnvironmentSetupFailure
    })?;
    let compute_budget = invoke_context.get_compute_budget();
    let config = Config {
        max_call_depth: compute_budget.max_call_depth,
        stack_frame_size: compute_budget.stack_frame_size,
        enable_instruction_tracing: log_enabled!(Trace),
        verify_mul64_imm_nonzero: !invoke_context
            .is_feature_active(&stop_verify_mul64_imm_nonzero::id()), // TODO: Feature gate and then remove me
        ..Config::default()
    };
    let mut executable = {
        let keyed_accounts = invoke_context.get_keyed_accounts()?;
        let programdata = keyed_account_at_index(keyed_accounts, programdata_account_index)?;
        <dyn Executable<BpfError, ThisInstructionMeter>>::from_elf(
            &programdata.try_account_ref()?.data()[programdata_offset..],
            None,
            config,
            syscall_registry,
        )
    }
    .map_err(|e| map_ebpf_error(invoke_context, e))?;
    let text_bytes = executable.get_text_bytes().1;
    verifier::check(text_bytes, &config)
        .map_err(|e| map_ebpf_error(invoke_context, EbpfError::UserError(e.into())))?;
    if use_jit {
        if let Err(err) = executable.jit_compile() {
            ic_msg!(invoke_context, "Failed to compile program {:?}", err);
            return Err(InstructionError::ProgramFailedToCompile);
        }
    }
    Ok(Arc::new(BpfExecutor { executable }))
}

fn write_program_data(
    program_account_index: usize,
    program_data_offset: usize,
    bytes: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    let keyed_accounts = invoke_context.get_keyed_accounts()?;
    let program = keyed_account_at_index(keyed_accounts, program_account_index)?;
    let mut account = program.try_account_ref_mut()?;
    let data = &mut account.data_as_mut_slice();
    let len = bytes.len();
    if data.len() < program_data_offset + len {
        ic_msg!(
            invoke_context,
            "Write overflow: {} < {}",
            data.len(),
            program_data_offset + len
        );
        return Err(InstructionError::AccountDataTooSmall);
    }
    data[program_data_offset..program_data_offset + len].copy_from_slice(bytes);
    Ok(())
}

fn check_loader_id(id: &Pubkey) -> bool {
    bpf_loader::check_id(id)
        || bpf_loader_deprecated::check_id(id)
        || bpf_loader_upgradeable::check_id(id)
}

/// Create the BPF virtual machine
pub fn create_vm<'a>(
    loader_id: &'a Pubkey,
    program: &'a dyn Executable<BpfError, ThisInstructionMeter>,
    parameter_bytes: &mut [u8],
    invoke_context: &'a mut dyn InvokeContext,
    orig_data_lens: &'a [usize],
) -> Result<EbpfVm<'a, BpfError, ThisInstructionMeter>, EbpfError<BpfError>> {
    let compute_budget = invoke_context.get_compute_budget();
    let mut heap =
        AlignedMemory::new_with_size(compute_budget.heap_size.unwrap_or(HEAP_LENGTH), HOST_ALIGN);
    let mut vm = EbpfVm::new(program, heap.as_slice_mut(), parameter_bytes)?;
    syscalls::bind_syscall_context_objects(
        loader_id,
        &mut vm,
        invoke_context,
        heap,
        orig_data_lens,
    )?;
    Ok(vm)
}

pub fn process_instruction(
    first_instruction_account: usize,
    instruction_data: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    process_instruction_common(
        first_instruction_account,
        instruction_data,
        invoke_context,
        false,
    )
}

pub fn process_instruction_jit(
    first_instruction_account: usize,
    instruction_data: &[u8],
    invoke_context: &mut dyn InvokeContext,
) -> Result<(), InstructionError> {
    process_instruction_common(
        first_instruction_account,
        instruction_data,
        invoke_context,
        true,
    )
}

fn process_instruction_common(
    first_instruction_account: usize,
    instruction_data: &[u8],
    invoke_context: &mut dyn InvokeContext,
    use_jit: bool,
) -> Result<(), InstructionError> {
    let logger = invoke_context.get_logger();
    let program_id = invoke_context.get_caller()?;

    let keyed_accounts = invoke_context.get_keyed_accounts()?;
    let first_account = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
    let second_account = keyed_account_at_index(keyed_accounts, first_instruction_account + 1);
    let (program, next_first_instruction_account) = if first_account.unsigned_key() == program_id {
        (first_account, first_instruction_account)
    } else if second_account
        .as_ref()
        .map(|keyed_account| keyed_account.unsigned_key() == program_id)
        .unwrap_or(false)
    {
        (second_account?, first_instruction_account + 1)
    } else {
        if first_account.executable()? {
            ic_logger_msg!(logger, "BPF loader is executable");
            return Err(InstructionError::IncorrectProgramId);
        }
        (first_account, first_instruction_account)
    };

    if program.executable()? {
        debug_assert_eq!(
            first_instruction_account,
            1 - (invoke_context.invoke_depth() > 1) as usize,
        );

        if !check_loader_id(&program.owner()?) {
            ic_logger_msg!(logger, "Executable account not owned by the BPF loader");
            return Err(InstructionError::IncorrectProgramId);
        }

        let program_data_offset = if bpf_loader_upgradeable::check_id(&program.owner()?) {
            if let UpgradeableLoaderState::Program {
                programdata_address,
            } = program.state()?
            {
                if programdata_address != *first_account.unsigned_key() {
                    ic_logger_msg!(logger, "Wrong ProgramData account for this Program account");
                    return Err(InstructionError::InvalidArgument);
                }
                if !matches!(
                    first_account.state()?,
                    UpgradeableLoaderState::ProgramData {
                        slot: _,
                        upgrade_authority_address: _,
                    }
                ) {
                    ic_logger_msg!(logger, "Program has been closed");
                    return Err(InstructionError::InvalidAccountData);
                }
                UpgradeableLoaderState::programdata_data_offset()?
            } else {
                ic_logger_msg!(logger, "Invalid Program account");
                return Err(InstructionError::InvalidAccountData);
            }
        } else {
            0
        };

        let executor = match invoke_context.get_executor(program_id) {
            Some(executor) => executor,
            None => {
                let executor = create_executor(
                    first_instruction_account,
                    program_data_offset,
                    invoke_context,
                    use_jit,
                )?;
                let program_id = invoke_context.get_caller()?;
                invoke_context.add_executor(program_id, executor.clone());
                executor
            }
        };
        executor.execute(
            next_first_instruction_account,
            instruction_data,
            invoke_context,
            use_jit,
        )
    } else {
        debug_assert_eq!(first_instruction_account, 1);
        if bpf_loader_upgradeable::check_id(program_id) {
            process_loader_upgradeable_instruction(
                first_instruction_account,
                instruction_data,
                invoke_context,
                use_jit,
            )
        } else if bpf_loader::check_id(program_id) || bpf_loader_deprecated::check_id(program_id) {
            process_loader_instruction(
                first_instruction_account,
                instruction_data,
                invoke_context,
                use_jit,
            )
        } else {
            ic_logger_msg!(logger, "Invalid BPF loader id");
            Err(InstructionError::IncorrectProgramId)
        }
    }
}

fn process_loader_upgradeable_instruction(
    first_instruction_account: usize,
    instruction_data: &[u8],
    invoke_context: &mut dyn InvokeContext,
    use_jit: bool,
) -> Result<(), InstructionError> {
    let logger = invoke_context.get_logger();
    let program_id = invoke_context.get_caller()?;
    let keyed_accounts = invoke_context.get_keyed_accounts()?;

    match limited_deserialize(instruction_data)? {
        UpgradeableLoaderInstruction::InitializeBuffer => {
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account)?;

            if UpgradeableLoaderState::Uninitialized != buffer.state()? {
                ic_logger_msg!(logger, "Buffer account already initialized");
                return Err(InstructionError::AccountAlreadyInitialized);
            }

            let authority = keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;

            buffer.set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(*authority.unsigned_key()),
            })?;
        }
        UpgradeableLoaderInstruction::Write { offset, bytes } => {
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let authority = keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;

            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.state()? {
                if authority_address.is_none() {
                    ic_logger_msg!(logger, "Buffer is immutable");
                    return Err(InstructionError::Immutable); // TODO better error code
                }
                if authority_address != Some(*authority.unsigned_key()) {
                    ic_logger_msg!(logger, "Incorrect buffer authority provided");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if authority.signer_key().is_none() {
                    ic_logger_msg!(logger, "Buffer authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(logger, "Invalid Buffer account");
                return Err(InstructionError::InvalidAccountData);
            }
            write_program_data(
                first_instruction_account,
                UpgradeableLoaderState::buffer_data_offset()? + offset as usize,
                &bytes,
                invoke_context,
            )?;
        }
        UpgradeableLoaderInstruction::DeployWithMaxDataLen { max_data_len } => {
            let payer = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let programdata =
                keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;
            let program = keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account + 3)?;
            let rent = from_keyed_account::<Rent>(keyed_account_at_index(
                keyed_accounts,
                first_instruction_account + 4,
            )?)?;
            let clock = from_keyed_account::<Clock>(keyed_account_at_index(
                keyed_accounts,
                first_instruction_account + 5,
            )?)?;
            let authority = keyed_account_at_index(keyed_accounts, first_instruction_account + 7)?;
            let upgrade_authority_address = Some(*authority.unsigned_key());
            let upgrade_authority_signer = authority.signer_key().is_none();

            // Verify Program account

            if UpgradeableLoaderState::Uninitialized != program.state()? {
                ic_logger_msg!(logger, "Program account already initialized");
                return Err(InstructionError::AccountAlreadyInitialized);
            }
            if program.data_len()? < UpgradeableLoaderState::program_len()? {
                ic_logger_msg!(logger, "Program account too small");
                return Err(InstructionError::AccountDataTooSmall);
            }
            if program.lamports()? < rent.minimum_balance(program.data_len()?) {
                ic_logger_msg!(logger, "Program account not rent-exempt");
                return Err(InstructionError::ExecutableAccountNotRentExempt);
            }

            let new_program_id = *program.unsigned_key();

            // Verify Buffer account

            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.state()? {
                if authority_address != upgrade_authority_address {
                    ic_logger_msg!(logger, "Buffer and upgrade authority don't match");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if upgrade_authority_signer {
                    ic_logger_msg!(logger, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(logger, "Invalid Buffer account");
                return Err(InstructionError::InvalidArgument);
            }

            let buffer_data_offset = UpgradeableLoaderState::buffer_data_offset()?;
            let buffer_data_len = buffer.data_len()?.saturating_sub(buffer_data_offset);
            let programdata_data_offset = UpgradeableLoaderState::programdata_data_offset()?;
            let programdata_len = UpgradeableLoaderState::programdata_len(max_data_len)?;

            if buffer.data_len()? < UpgradeableLoaderState::buffer_data_offset()?
                || buffer_data_len == 0
            {
                ic_logger_msg!(logger, "Buffer account too small");
                return Err(InstructionError::InvalidAccountData);
            }
            if max_data_len < buffer_data_len {
                ic_logger_msg!(logger, "Max data length is too small to hold Buffer data");
                return Err(InstructionError::AccountDataTooSmall);
            }
            if programdata_len > MAX_PERMITTED_DATA_LENGTH as usize {
                ic_logger_msg!(logger, "Max data length is too large");
                return Err(InstructionError::InvalidArgument);
            }

            // Create ProgramData account

            let (derived_address, bump_seed) =
                Pubkey::find_program_address(&[new_program_id.as_ref()], program_id);
            if derived_address != *programdata.unsigned_key() {
                ic_logger_msg!(logger, "ProgramData address is not derived");
                return Err(InstructionError::InvalidArgument);
            }

            let predrain_buffer = invoke_context
                .is_feature_active(&reduce_required_deploy_balance::id())
                && invoke_context.is_feature_active(&fix_write_privs::id());
            if predrain_buffer {
                // Drain the Buffer account to payer before paying for programdata account
                payer
                    .try_account_ref_mut()?
                    .checked_add_lamports(buffer.lamports()?)?;
                buffer.try_account_ref_mut()?.set_lamports(0);
            }

            let mut instruction = system_instruction::create_account(
                payer.unsigned_key(),
                programdata.unsigned_key(),
                1.max(rent.minimum_balance(programdata_len)),
                programdata_len as u64,
                program_id,
            );

            // pass an extra account to avoid the overly strict UnbalancedInstruction error
            instruction
                .accounts
                .push(AccountMeta::new(*buffer.unsigned_key(), false));

            let caller_program_id = invoke_context.get_caller()?;
            let signers = [&[new_program_id.as_ref(), &[bump_seed]]]
                .iter()
                .map(|seeds| Pubkey::create_program_address(*seeds, caller_program_id))
                .collect::<Result<Vec<Pubkey>, solana_sdk::pubkey::PubkeyError>>()?;
            InstructionProcessor::native_invoke(
                invoke_context,
                instruction,
                &[0, 1, 3, 6],
                signers.as_slice(),
            )?;

            // Load and verify the program bits
            let executor = create_executor(
                first_instruction_account + 3,
                buffer_data_offset,
                invoke_context,
                use_jit,
            )?;
            invoke_context.add_executor(&new_program_id, executor);

            let keyed_accounts = invoke_context.get_keyed_accounts()?;
            let payer = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let programdata =
                keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;
            let program = keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account + 3)?;

            // Update the ProgramData account and record the program bits
            programdata.set_state(&UpgradeableLoaderState::ProgramData {
                slot: clock.slot,
                upgrade_authority_address,
            })?;
            programdata.try_account_ref_mut()?.data_as_mut_slice()
                [programdata_data_offset..programdata_data_offset + buffer_data_len]
                .copy_from_slice(&buffer.try_account_ref()?.data()[buffer_data_offset..]);

            // Update the Program account
            program.set_state(&UpgradeableLoaderState::Program {
                programdata_address: *programdata.unsigned_key(),
            })?;
            program.try_account_ref_mut()?.set_executable(true);

            if !predrain_buffer {
                // Drain the Buffer account back to the payer
                payer
                    .try_account_ref_mut()?
                    .checked_add_lamports(buffer.lamports()?)?;
                buffer.try_account_ref_mut()?.set_lamports(0);
            }

            ic_logger_msg!(logger, "Deployed program {:?}", new_program_id);
        }
        UpgradeableLoaderInstruction::Upgrade => {
            let programdata = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let program = keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;
            let rent = from_keyed_account::<Rent>(keyed_account_at_index(
                keyed_accounts,
                first_instruction_account + 4,
            )?)?;
            let clock = from_keyed_account::<Clock>(keyed_account_at_index(
                keyed_accounts,
                first_instruction_account + 5,
            )?)?;
            let authority = keyed_account_at_index(keyed_accounts, first_instruction_account + 6)?;

            // Verify Program account

            if !program.executable()? {
                ic_logger_msg!(logger, "Program account not executable");
                return Err(InstructionError::AccountNotExecutable);
            }
            if !program.is_writable() {
                ic_logger_msg!(logger, "Program account not writeable");
                return Err(InstructionError::InvalidArgument);
            }
            if &program.owner()? != program_id {
                ic_logger_msg!(logger, "Program account not owned by loader");
                return Err(InstructionError::IncorrectProgramId);
            }
            if let UpgradeableLoaderState::Program {
                programdata_address,
            } = program.state()?
            {
                if programdata_address != *programdata.unsigned_key() {
                    ic_logger_msg!(logger, "Program and ProgramData account mismatch");
                    return Err(InstructionError::InvalidArgument);
                }
            } else {
                ic_logger_msg!(logger, "Invalid Program account");
                return Err(InstructionError::InvalidAccountData);
            }

            let new_program_id = *program.unsigned_key();

            // Verify Buffer account

            if let UpgradeableLoaderState::Buffer { authority_address } = buffer.state()? {
                if authority_address != Some(*authority.unsigned_key()) {
                    ic_logger_msg!(logger, "Buffer and upgrade authority don't match");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if authority.signer_key().is_none() {
                    ic_logger_msg!(logger, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(logger, "Invalid Buffer account");
                return Err(InstructionError::InvalidArgument);
            }

            let buffer_data_offset = UpgradeableLoaderState::buffer_data_offset()?;
            let buffer_data_len = buffer.data_len()?.saturating_sub(buffer_data_offset);
            let programdata_data_offset = UpgradeableLoaderState::programdata_data_offset()?;
            let programdata_balance_required = 1.max(rent.minimum_balance(programdata.data_len()?));

            if buffer.data_len()? < UpgradeableLoaderState::buffer_data_offset()?
                || buffer_data_len == 0
            {
                ic_logger_msg!(logger, "Buffer account too small");
                return Err(InstructionError::InvalidAccountData);
            }

            // Verify ProgramData account

            if programdata.data_len()? < UpgradeableLoaderState::programdata_len(buffer_data_len)? {
                ic_logger_msg!(logger, "ProgramData account not large enough");
                return Err(InstructionError::AccountDataTooSmall);
            }
            if programdata.lamports()? + buffer.lamports()? < programdata_balance_required {
                ic_logger_msg!(logger, "Buffer account balance too low to fund upgrade");
                return Err(InstructionError::InsufficientFunds);
            }
            if let UpgradeableLoaderState::ProgramData {
                slot: _,
                upgrade_authority_address,
            } = programdata.state()?
            {
                if upgrade_authority_address.is_none() {
                    ic_logger_msg!(logger, "Program not upgradeable");
                    return Err(InstructionError::Immutable);
                }
                if upgrade_authority_address != Some(*authority.unsigned_key()) {
                    ic_logger_msg!(logger, "Incorrect upgrade authority provided");
                    return Err(InstructionError::IncorrectAuthority);
                }
                if authority.signer_key().is_none() {
                    ic_logger_msg!(logger, "Upgrade authority did not sign");
                    return Err(InstructionError::MissingRequiredSignature);
                }
            } else {
                ic_logger_msg!(logger, "Invalid ProgramData account");
                return Err(InstructionError::InvalidAccountData);
            }

            // Load and verify the program bits
            let executor = create_executor(
                first_instruction_account + 2,
                buffer_data_offset,
                invoke_context,
                use_jit,
            )?;
            invoke_context.add_executor(&new_program_id, executor);

            let keyed_accounts = invoke_context.get_keyed_accounts()?;
            let programdata = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let buffer = keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;
            let spill = keyed_account_at_index(keyed_accounts, first_instruction_account + 3)?;
            let authority = keyed_account_at_index(keyed_accounts, first_instruction_account + 6)?;

            // Update the ProgramData account, record the upgraded data, and zero
            // the rest
            programdata.set_state(&UpgradeableLoaderState::ProgramData {
                slot: clock.slot,
                upgrade_authority_address: Some(*authority.unsigned_key()),
            })?;
            programdata.try_account_ref_mut()?.data_as_mut_slice()
                [programdata_data_offset..programdata_data_offset + buffer_data_len]
                .copy_from_slice(&buffer.try_account_ref()?.data()[buffer_data_offset..]);
            programdata.try_account_ref_mut()?.data_as_mut_slice()
                [programdata_data_offset + buffer_data_len..]
                .fill(0);

            // Fund ProgramData to rent-exemption, spill the rest

            spill.try_account_ref_mut()?.checked_add_lamports(
                (programdata.lamports()? + buffer.lamports()?)
                    .saturating_sub(programdata_balance_required),
            )?;
            buffer.try_account_ref_mut()?.set_lamports(0);
            programdata
                .try_account_ref_mut()?
                .set_lamports(programdata_balance_required);

            ic_logger_msg!(logger, "Upgraded program {:?}", new_program_id);
        }
        UpgradeableLoaderInstruction::SetAuthority => {
            let account = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let present_authority =
                keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;
            let new_authority =
                keyed_account_at_index(keyed_accounts, first_instruction_account + 2)
                    .ok()
                    .map(|account| account.unsigned_key());

            match account.state()? {
                UpgradeableLoaderState::Buffer { authority_address } => {
                    if new_authority.is_none() {
                        ic_logger_msg!(logger, "Buffer authority is not optional");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if authority_address.is_none() {
                        ic_logger_msg!(logger, "Buffer is immutable");
                        return Err(InstructionError::Immutable);
                    }
                    if authority_address != Some(*present_authority.unsigned_key()) {
                        ic_logger_msg!(logger, "Incorrect buffer authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if present_authority.signer_key().is_none() {
                        ic_logger_msg!(logger, "Buffer authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    account.set_state(&UpgradeableLoaderState::Buffer {
                        authority_address: new_authority.cloned(),
                    })?;
                }
                UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address,
                } => {
                    if upgrade_authority_address.is_none() {
                        ic_logger_msg!(logger, "Program not upgradeable");
                        return Err(InstructionError::Immutable);
                    }
                    if upgrade_authority_address != Some(*present_authority.unsigned_key()) {
                        ic_logger_msg!(logger, "Incorrect upgrade authority provided");
                        return Err(InstructionError::IncorrectAuthority);
                    }
                    if present_authority.signer_key().is_none() {
                        ic_logger_msg!(logger, "Upgrade authority did not sign");
                        return Err(InstructionError::MissingRequiredSignature);
                    }
                    account.set_state(&UpgradeableLoaderState::ProgramData {
                        slot,
                        upgrade_authority_address: new_authority.cloned(),
                    })?;
                }
                _ => {
                    ic_logger_msg!(logger, "Account does not support authorities");
                    return Err(InstructionError::InvalidArgument);
                }
            }

            ic_logger_msg!(logger, "New authority {:?}", new_authority);
        }
        UpgradeableLoaderInstruction::Close => {
            let close_account = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            let recipient_account =
                keyed_account_at_index(keyed_accounts, first_instruction_account + 1)?;
            if !invoke_context.is_feature_active(&close_upgradeable_program_accounts::id()) {
                let _ = keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;
            }

            if close_account.unsigned_key() == recipient_account.unsigned_key() {
                ic_logger_msg!(logger, "Recipient is the same as the account being closed");
                return Err(InstructionError::InvalidArgument);
            }

            if !invoke_context.is_feature_active(&close_upgradeable_program_accounts::id())
                && !matches!(
                    close_account.state()?,
                    UpgradeableLoaderState::Buffer {
                        authority_address: _,
                    }
                )
            {
                ic_logger_msg!(logger, "Account does not support closing");
                return Err(InstructionError::InvalidArgument);
            }

            match close_account.state()? {
                UpgradeableLoaderState::Uninitialized => {
                    recipient_account
                        .try_account_ref_mut()?
                        .checked_add_lamports(close_account.lamports()?)?;
                    close_account.try_account_ref_mut()?.set_lamports(0);

                    ic_logger_msg!(
                        logger,
                        "Closed Uninitialized {}",
                        close_account.unsigned_key()
                    );
                }
                UpgradeableLoaderState::Buffer { authority_address } => {
                    let authority =
                        keyed_account_at_index(keyed_accounts, first_instruction_account + 2)?;

                    common_close_account(
                        &authority_address,
                        authority,
                        close_account,
                        recipient_account,
                        logger.clone(),
                        !invoke_context
                            .is_feature_active(&close_upgradeable_program_accounts::id()),
                    )?;

                    ic_logger_msg!(logger, "Closed Buffer {}", close_account.unsigned_key());
                }
                UpgradeableLoaderState::ProgramData {
                    slot: _,
                    upgrade_authority_address: authority_address,
                } => {
                    let program_account =
                        keyed_account_at_index(keyed_accounts, first_instruction_account + 3)?;

                    if !program_account.is_writable() {
                        ic_logger_msg!(logger, "Program account is not writable");
                        return Err(InstructionError::InvalidArgument);
                    }
                    if &program_account.owner()? != program_id {
                        ic_logger_msg!(logger, "Program account not owned by loader");
                        return Err(InstructionError::IncorrectProgramId);
                    }

                    match program_account.state()? {
                        UpgradeableLoaderState::Program {
                            programdata_address,
                        } => {
                            if programdata_address != *close_account.unsigned_key() {
                                ic_logger_msg!(
                                    logger,
                                    "ProgramData account does not match ProgramData account"
                                );
                                return Err(InstructionError::InvalidArgument);
                            }

                            let authority = keyed_account_at_index(
                                keyed_accounts,
                                first_instruction_account + 2,
                            )?;
                            common_close_account(
                                &authority_address,
                                authority,
                                close_account,
                                recipient_account,
                                logger.clone(),
                                !invoke_context
                                    .is_feature_active(&close_upgradeable_program_accounts::id()),
                            )?;
                        }
                        _ => {
                            ic_logger_msg!(logger, "Invalid Program account");
                            return Err(InstructionError::InvalidArgument);
                        }
                    }

                    ic_logger_msg!(logger, "Closed Program {}", program_account.unsigned_key());
                }
                _ => {
                    ic_logger_msg!(logger, "Account does not support closing");
                    return Err(InstructionError::InvalidArgument);
                }
            }
        }
    }

    Ok(())
}

fn common_close_account(
    authority_address: &Option<Pubkey>,
    authority_account: &KeyedAccount,
    close_account: &KeyedAccount,
    recipient_account: &KeyedAccount,
    logger: Rc<RefCell<dyn Logger>>,
    do_clear_data: bool,
) -> Result<(), InstructionError> {
    if authority_address.is_none() {
        ic_logger_msg!(logger, "Account is immutable");
        return Err(InstructionError::Immutable);
    }
    if *authority_address != Some(*authority_account.unsigned_key()) {
        ic_logger_msg!(logger, "Incorrect authority provided");
        return Err(InstructionError::IncorrectAuthority);
    }
    if authority_account.signer_key().is_none() {
        ic_logger_msg!(logger, "Authority did not sign");
        return Err(InstructionError::MissingRequiredSignature);
    }

    recipient_account
        .try_account_ref_mut()?
        .checked_add_lamports(close_account.lamports()?)?;
    close_account.try_account_ref_mut()?.set_lamports(0);
    if do_clear_data {
        for elt in close_account.try_account_ref_mut()?.data_as_mut_slice() {
            *elt = 0;
        }
    } else {
        close_account.set_state(&UpgradeableLoaderState::Uninitialized)?;
    }
    Ok(())
}

fn process_loader_instruction(
    first_instruction_account: usize,
    instruction_data: &[u8],
    invoke_context: &mut dyn InvokeContext,
    use_jit: bool,
) -> Result<(), InstructionError> {
    let program_id = invoke_context.get_caller()?;
    let keyed_accounts = invoke_context.get_keyed_accounts()?;
    let program = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
    if program.owner()? != *program_id {
        ic_msg!(
            invoke_context,
            "Executable account not owned by the BPF loader"
        );
        return Err(InstructionError::IncorrectProgramId);
    }
    match limited_deserialize(instruction_data)? {
        LoaderInstruction::Write { offset, bytes } => {
            if program.signer_key().is_none() {
                ic_msg!(invoke_context, "Program account did not sign");
                return Err(InstructionError::MissingRequiredSignature);
            }
            write_program_data(
                first_instruction_account,
                offset as usize,
                &bytes,
                invoke_context,
            )?;
        }
        LoaderInstruction::Finalize => {
            if program.signer_key().is_none() {
                ic_msg!(invoke_context, "key[0] did not sign the transaction");
                return Err(InstructionError::MissingRequiredSignature);
            }

            let executor = create_executor(first_instruction_account, 0, invoke_context, use_jit)?;
            let keyed_accounts = invoke_context.get_keyed_accounts()?;
            let program = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
            invoke_context.add_executor(program.unsigned_key(), executor);
            program.try_account_ref_mut()?.set_executable(true);
            ic_msg!(
                invoke_context,
                "Finalized account {:?}",
                program.unsigned_key()
            );
        }
    }

    Ok(())
}

/// Passed to the VM to enforce the compute budget
pub struct ThisInstructionMeter {
    pub compute_meter: Rc<RefCell<dyn ComputeMeter>>,
}
impl ThisInstructionMeter {
    fn new(compute_meter: Rc<RefCell<dyn ComputeMeter>>) -> Self {
        Self { compute_meter }
    }
}
impl InstructionMeter for ThisInstructionMeter {
    fn consume(&mut self, amount: u64) {
        // 1 to 1 instruction to compute unit mapping
        // ignore error, Ebpf will bail if exceeded
        let _ = self.compute_meter.borrow_mut().consume(amount);
    }
    fn get_remaining(&self) -> u64 {
        self.compute_meter.borrow().get_remaining()
    }
}

/// BPF Loader's Executor implementation
pub struct BpfExecutor {
    executable: Box<dyn Executable<BpfError, ThisInstructionMeter>>,
}

// Well, implement Debug for solana_rbpf::vm::Executable in solana-rbpf...
impl Debug for BpfExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "BpfExecutor({:p})", self)
    }
}

impl Executor for BpfExecutor {
    fn execute(
        &self,
        first_instruction_account: usize,
        instruction_data: &[u8],
        invoke_context: &mut dyn InvokeContext,
        use_jit: bool,
    ) -> Result<(), InstructionError> {
        let logger = invoke_context.get_logger();
        let invoke_depth = invoke_context.invoke_depth();
        let add_missing_program_error_mappings =
            invoke_context.is_feature_active(&add_missing_program_error_mappings::id());

        let mut serialize_time = Measure::start("serialize");
        let keyed_accounts = invoke_context.get_keyed_accounts()?;
        let program = keyed_account_at_index(keyed_accounts, first_instruction_account)?;
        let loader_id = &program.owner()?;
        let program_id = invoke_context.get_caller()?;
        let (mut parameter_bytes, account_lengths) = serialize_parameters(
            loader_id,
            program_id,
            &keyed_accounts[first_instruction_account + 1..],
            instruction_data,
        )?;
        serialize_time.stop();
        let mut create_vm_time = Measure::start("create_vm");
        let mut execute_time;
        {
            let program_id = &invoke_context.get_caller()?.clone();
            let compute_meter = invoke_context.get_compute_meter();
            let mut vm = match create_vm(
                loader_id,
                self.executable.as_ref(),
                parameter_bytes.as_slice_mut(),
                invoke_context,
                &account_lengths,
            ) {
                Ok(info) => info,
                Err(e) => {
                    ic_logger_msg!(logger, "Failed to create BPF VM: {}", e);
                    return Err(InstructionError::ProgramEnvironmentSetupFailure);
                }
            };
            create_vm_time.stop();

            execute_time = Measure::start("execute");
            stable_log::program_invoke(&logger, program_id, invoke_depth);
            let mut instruction_meter = ThisInstructionMeter::new(compute_meter.clone());
            let before = compute_meter.borrow().get_remaining();
            let result = if use_jit {
                vm.execute_program_jit(&mut instruction_meter)
            } else {
                vm.execute_program_interpreted(&mut instruction_meter)
            };
            let after = compute_meter.borrow().get_remaining();
            ic_logger_msg!(
                logger,
                "Program {} consumed {} of {} compute units",
                program_id,
                before - after,
                before
            );
            if log_enabled!(Trace) {
                let mut trace_buffer = Vec::<u8>::new();
                let analysis = Analysis::from_executable(self.executable.as_ref());
                vm.get_tracer().write(&mut trace_buffer, &analysis).unwrap();
                let trace_string = String::from_utf8(trace_buffer).unwrap();
                trace!("BPF Program Instruction Trace:\n{}", trace_string);
            }
            drop(vm);
            let (program_id, return_data) = invoke_context.get_return_data();
            if !return_data.is_empty() {
                stable_log::program_return(&logger, &program_id, return_data);
            }
            match result {
                Ok(status) => {
                    if status != SUCCESS {
                        let error: InstructionError = if !add_missing_program_error_mappings
                            && (status == ACCOUNT_NOT_RENT_EXEMPT || status == BORSH_IO_ERROR)
                        {
                            // map originally missing error mappings to InvalidError
                            InstructionError::InvalidError
                        } else {
                            status.into()
                        };
                        stable_log::program_failure(&logger, &program_id, &error);
                        return Err(error);
                    }
                }
                Err(error) => {
                    let error = match error {
                        EbpfError::UserError(BpfError::SyscallError(
                            SyscallError::InstructionError(error),
                        )) => error,
                        err => {
                            ic_logger_msg!(logger, "Program failed to complete: {}", err);
                            InstructionError::ProgramFailedToComplete
                        }
                    };
                    stable_log::program_failure(&logger, &program_id, &error);
                    return Err(error);
                }
            }
            execute_time.stop();
        }
        let mut deserialize_time = Measure::start("deserialize");
        let keyed_accounts = invoke_context.get_keyed_accounts()?;
        deserialize_parameters(
            loader_id,
            &keyed_accounts[first_instruction_account + 1..],
            parameter_bytes.as_slice(),
            &account_lengths,
            invoke_context.is_feature_active(&do_support_realloc::id()),
        )?;
        deserialize_time.stop();
        invoke_context.update_timing(
            serialize_time.as_us(),
            create_vm_time.as_us(),
            execute_time.as_us(),
            deserialize_time.as_us(),
        );
        let program_id = invoke_context.get_caller()?;
        stable_log::program_success(&logger, program_id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::Rng;
    use solana_rbpf::vm::SyscallRegistry;
    use solana_runtime::{bank::Bank, bank_client::BankClient};
    use solana_sdk::{
        account::{
            create_account_shared_data_for_test as create_account_for_test, AccountSharedData,
        },
        account_utils::StateMut,
        client::SyncClient,
        clock::Clock,
        feature_set::FeatureSet,
        genesis_config::create_genesis_config,
        instruction::Instruction,
        instruction::{AccountMeta, InstructionError},
        keyed_account::create_keyed_accounts_unified,
        message::Message,
        native_loader,
        native_token::LAMPORTS_PER_SOL,
        process_instruction::{MockComputeMeter, MockInvokeContext},
        pubkey::Pubkey,
        rent::Rent,
        signature::{Keypair, Signer},
        system_program, sysvar,
        transaction::TransactionError,
    };
    use std::{cell::RefCell, fs::File, io::Read, ops::Range, rc::Rc, sync::Arc};

    struct TestInstructionMeter {
        remaining: u64,
    }
    impl InstructionMeter for TestInstructionMeter {
        fn consume(&mut self, amount: u64) {
            self.remaining = self.remaining.saturating_sub(amount);
        }
        fn get_remaining(&self) -> u64 {
            self.remaining
        }
    }

    type KeyedAccountTuple<'a> = (bool, bool, &'a Pubkey, &'a RefCell<AccountSharedData>);
    fn process_instruction(
        owner: &Pubkey,
        instruction_data: &[u8],
        keyed_accounts: &[(bool, bool, &Pubkey, &RefCell<AccountSharedData>)],
    ) -> Result<(), InstructionError> {
        let processor_account = AccountSharedData::new_ref(0, 0, &native_loader::id());
        let mut keyed_accounts = keyed_accounts.to_vec();
        keyed_accounts.insert(0, (false, false, owner, &processor_account));
        super::process_instruction(
            1,
            instruction_data,
            &mut MockInvokeContext::new(owner, create_keyed_accounts_unified(&keyed_accounts)),
        )
    }

    fn load_program_account_from_elf(
        loader_id: &Pubkey,
        path: &str,
    ) -> Rc<RefCell<AccountSharedData>> {
        let mut file = File::open(path).expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();
        let rent = Rent::default();
        let program_account =
            AccountSharedData::new_ref(rent.minimum_balance(elf.len()), 0, loader_id);
        program_account.borrow_mut().set_data(elf);
        program_account.borrow_mut().set_executable(true);
        program_account
    }

    #[test]
    #[should_panic(expected = "ExceededMaxInstructions(31, 10)")]
    fn test_bpf_loader_non_terminating_program() {
        #[rustfmt::skip]
        let program = &[
            0x07, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, // r6 + 1
            0x05, 0x00, 0xfe, 0xff, 0x00, 0x00, 0x00, 0x00, // goto -2
            0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // exit
        ];
        let input = &mut [0x00];
        let mut bpf_functions = std::collections::BTreeMap::<u32, (usize, String)>::new();
        solana_rbpf::elf::register_bpf_function(&mut bpf_functions, 0, "entrypoint").unwrap();
        let program = <dyn Executable<BpfError, TestInstructionMeter>>::from_text_bytes(
            program,
            None,
            Config::default(),
            SyscallRegistry::default(),
            bpf_functions,
        )
        .unwrap();
        let mut vm =
            EbpfVm::<BpfError, TestInstructionMeter>::new(program.as_ref(), &mut [], input)
                .unwrap();
        let mut instruction_meter = TestInstructionMeter { remaining: 10 };
        vm.execute_program_interpreted(&mut instruction_meter)
            .unwrap();
    }

    #[test]
    #[should_panic(expected = "LDDWCannotBeLast")]
    fn test_bpf_loader_check_load_dw() {
        let prog = &[
            0x18, 0x00, 0x00, 0x00, 0x88, 0x77, 0x66, 0x55, // first half of lddw
        ];
        verifier::check(prog, &Config::default()).unwrap();
    }

    #[test]
    fn test_bpf_loader_write() {
        let loader_id = bpf_loader::id();
        let program_key = solana_sdk::pubkey::new_rand();
        let program_account = AccountSharedData::new_ref(1, 0, &loader_id);
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![];
        let instruction_data = bincode::serialize(&LoaderInstruction::Write {
            offset: 3,
            bytes: vec![1, 2, 3],
        })
        .unwrap();

        // Case: No program account
        assert_eq!(
            Err(InstructionError::NotEnoughAccountKeys),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );

        // Case: Not signed
        keyed_accounts.push((false, false, &program_key, &program_account));
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );

        // Case: Write bytes to an offset
        keyed_accounts[0] = (true, false, &program_key, &program_account);
        keyed_accounts[0].3.borrow_mut().set_data(vec![0; 6]);
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );
        assert_eq!(&vec![0, 0, 0, 1, 2, 3], keyed_accounts[0].3.borrow().data());

        // Case: Overflow
        keyed_accounts[0].3.borrow_mut().set_data(vec![0; 5]);
        assert_eq!(
            Err(InstructionError::AccountDataTooSmall),
            process_instruction(&bpf_loader::id(), &instruction_data, &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_finalize() {
        let loader_id = bpf_loader::id();
        let program_key = solana_sdk::pubkey::new_rand();
        let program_account =
            load_program_account_from_elf(&loader_id, "test_elfs/noop_aligned.so");
        program_account.borrow_mut().set_executable(false);

        // Case: No program account
        let instruction_data = bincode::serialize(&LoaderInstruction::Finalize).unwrap();
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![];
        assert_eq!(
            Err(InstructionError::NotEnoughAccountKeys),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );

        // Case: Not signed
        keyed_accounts.push((false, false, &program_key, &program_account));
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );

        // Case: Finalize
        keyed_accounts[0] = (true, false, &program_key, &program_account);
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );
        assert!(keyed_accounts[0].3.borrow().executable());

        program_account.borrow_mut().set_executable(false); // Un-finalize the account

        // Case: Finalize
        program_account.borrow_mut().data_as_mut_slice()[0] = 0; // bad elf
        assert_eq!(
            Err(InstructionError::InvalidAccountData),
            process_instruction(&loader_id, &instruction_data, &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_invoke_main() {
        let loader_id = bpf_loader::id();
        let program_key = solana_sdk::pubkey::new_rand();
        let program_account =
            load_program_account_from_elf(&loader_id, "test_elfs/noop_aligned.so");

        // Case: No program account
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![];
        assert_eq!(
            Err(InstructionError::NotEnoughAccountKeys),
            process_instruction(&loader_id, &[], &keyed_accounts),
        );

        // Case: Only a program account
        keyed_accounts.push((false, false, &program_key, &program_account));
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );

        // Case: Account not a program
        keyed_accounts[0].3.borrow_mut().set_executable(false);
        assert_eq!(
            Err(InstructionError::InvalidInstructionData),
            process_instruction(&loader_id, &[], &keyed_accounts),
        );
        keyed_accounts[0].3.borrow_mut().set_executable(true);

        // Case: With program and parameter account
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        keyed_accounts.push((false, false, &program_key, &parameter_account));
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );

        // Case: With duplicate accounts
        let duplicate_key = solana_sdk::pubkey::new_rand();
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        keyed_accounts[1] = (false, false, &duplicate_key, &parameter_account);
        keyed_accounts.push((false, false, &duplicate_key, &parameter_account));
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );

        // Case: limited budget
        let processor_account = AccountSharedData::new_ref(0, 0, &native_loader::id());
        let mut keyed_accounts = keyed_accounts.clone();
        keyed_accounts.insert(0, (false, false, &program_key, &processor_account));
        let mut invoke_context =
            MockInvokeContext::new(&program_key, create_keyed_accounts_unified(&keyed_accounts));
        invoke_context.compute_meter = MockComputeMeter::default();
        assert_eq!(
            Err(InstructionError::ProgramFailedToComplete),
            super::process_instruction(1, &[], &mut invoke_context)
        );
    }

    #[test]
    fn test_bpf_loader_serialize_unaligned() {
        let loader_id = bpf_loader_deprecated::id();
        let program_key = solana_sdk::pubkey::new_rand();
        let program_account =
            load_program_account_from_elf(&loader_id, "test_elfs/noop_unaligned.so");

        // Case: With program and parameter account
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &program_key, &program_account),
            (false, false, &program_key, &parameter_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );

        // Case: With duplicate accounts
        let duplicate_key = solana_sdk::pubkey::new_rand();
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        keyed_accounts[1] = (false, false, &duplicate_key, &parameter_account);
        keyed_accounts.push((false, false, &duplicate_key, &parameter_account));
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_serialize_aligned() {
        let loader_id = bpf_loader::id();
        let program_key = solana_sdk::pubkey::new_rand();
        let program_account =
            load_program_account_from_elf(&loader_id, "test_elfs/noop_aligned.so");

        // Case: With program and parameter account
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &program_key, &program_account),
            (false, false, &program_key, &parameter_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );

        // Case: With duplicate accounts
        let duplicate_key = solana_sdk::pubkey::new_rand();
        let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
        keyed_accounts[1] = (false, false, &duplicate_key, &parameter_account);
        keyed_accounts.push(keyed_accounts[1]);
        assert_eq!(
            Ok(()),
            process_instruction(&program_key, &[], &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_initialize_buffer() {
        let instruction =
            bincode::serialize(&UpgradeableLoaderInstruction::InitializeBuffer).unwrap();
        let loader_id = bpf_loader_upgradeable::id();
        let buffer_address = Pubkey::new_unique();
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(9).unwrap(),
            &loader_id,
        );
        let authority_address = Pubkey::new_unique();
        let authority_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(9).unwrap(),
            &loader_id,
        );

        // Case: Success
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (false, false, &authority_address, &authority_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address)
            }
        );

        // Case: Already initialized
        assert_eq!(
            Err(InstructionError::AccountAlreadyInitialized),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address)
            }
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_write() {
        let loader_id = bpf_loader_upgradeable::id();
        let buffer_address = Pubkey::new_unique();
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(9).unwrap(),
            &loader_id,
        );

        // Case: Not initialized
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![42; 9],
        })
        .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (true, false, &buffer_address, &buffer_account),
        ];
        assert_eq!(
            Err(InstructionError::InvalidAccountData),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Write entire buffer
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![42; 9],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address)
            }
        );
        assert_eq!(
            &buffer_account.borrow().data()
                [UpgradeableLoaderState::buffer_data_offset().unwrap()..],
            &[42; 9]
        );

        // Case: Write portion of the buffer
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 3,
            bytes: vec![42; 6],
        })
        .unwrap();
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(9).unwrap(),
            &loader_id,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (true, false, &buffer_address, &buffer_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address)
            }
        );
        assert_eq!(
            &buffer_account.borrow().data()
                [UpgradeableLoaderState::buffer_data_offset().unwrap()..],
            &[0, 0, 0, 42, 42, 42, 42, 42, 42]
        );

        // Case: overflow size
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![42; 10],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        assert_eq!(
            Err(InstructionError::AccountDataTooSmall),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: overflow offset
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 1,
            bytes: vec![42; 9],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        assert_eq!(
            Err(InstructionError::AccountDataTooSmall),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Not signed
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 0,
            bytes: vec![42; 9],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (false, false, &buffer_address, &buffer_account),
        ];
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: wrong authority
        let authority_address = Pubkey::new_unique();
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 1,
            bytes: vec![42; 9],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (true, false, &authority_address, &buffer_account),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: None authority
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Write {
            offset: 1,
            bytes: vec![42; 9],
        })
        .unwrap();
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: None,
            })
            .unwrap();
        assert_eq!(
            Err(InstructionError::Immutable),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
    }

    fn truncate_data(account: &mut AccountSharedData, len: usize) {
        let mut data = account.data().to_vec();
        data.truncate(len);
        account.set_data(data);
    }

    #[test]
    fn test_bpf_loader_upgradeable_deploy_with_max_len() {
        let (genesis_config, mint_keypair) = create_genesis_config(1_000_000_000);
        let mut bank = Bank::new_for_tests(&genesis_config);
        bank.feature_set = Arc::new(FeatureSet::all_enabled());
        bank.add_builtin(
            "solana_bpf_loader_upgradeable_program",
            &bpf_loader_upgradeable::id(),
            super::process_instruction,
        );
        let bank = Arc::new(bank);
        let bank_client = BankClient::new_shared(&bank);

        // Setup keypairs and addresses
        let payer_keypair = Keypair::new();
        let program_keypair = Keypair::new();
        let buffer_address = Pubkey::new_unique();
        let (programdata_address, _) = Pubkey::find_program_address(
            &[program_keypair.pubkey().as_ref()],
            &bpf_loader_upgradeable::id(),
        );
        let upgrade_authority_keypair = Keypair::new();

        // Load program file
        let mut file = File::open("test_elfs/noop_aligned.so").expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();

        // Compute rent exempt balances
        let program_len = elf.len();
        let min_program_balance = bank
            .get_minimum_balance_for_rent_exemption(UpgradeableLoaderState::program_len().unwrap());
        let min_buffer_balance = bank.get_minimum_balance_for_rent_exemption(
            UpgradeableLoaderState::buffer_len(program_len).unwrap(),
        );
        let min_programdata_balance = bank.get_minimum_balance_for_rent_exemption(
            UpgradeableLoaderState::programdata_len(program_len).unwrap(),
        );

        // Setup accounts
        let buffer_account = {
            let mut account = AccountSharedData::new(
                min_buffer_balance,
                UpgradeableLoaderState::buffer_len(elf.len()).unwrap(),
                &bpf_loader_upgradeable::id(),
            );
            account
                .set_state(&UpgradeableLoaderState::Buffer {
                    authority_address: Some(upgrade_authority_keypair.pubkey()),
                })
                .unwrap();
            account.data_as_mut_slice()[UpgradeableLoaderState::buffer_data_offset().unwrap()..]
                .copy_from_slice(&elf);
            account
        };
        let program_account = AccountSharedData::new(
            min_programdata_balance,
            UpgradeableLoaderState::program_len().unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        let programdata_account = AccountSharedData::new(
            1,
            UpgradeableLoaderState::programdata_len(elf.len()).unwrap(),
            &bpf_loader_upgradeable::id(),
        );

        // Test successful deploy
        let payer_base_balance = LAMPORTS_PER_SOL;
        let deploy_fees = {
            let fee_calculator = genesis_config.fee_rate_governor.create_fee_calculator();
            3 * fee_calculator.lamports_per_signature
        };
        let min_payer_balance =
            min_program_balance + min_programdata_balance - min_buffer_balance + deploy_fees;
        bank.store_account(
            &payer_keypair.pubkey(),
            &AccountSharedData::new(
                payer_base_balance + min_payer_balance,
                0,
                &system_program::id(),
            ),
        );
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &payer_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&payer_keypair.pubkey()),
        );
        assert!(bank_client
            .send_and_confirm_message(
                &[&payer_keypair, &program_keypair, &upgrade_authority_keypair],
                message
            )
            .is_ok());
        assert_eq!(
            bank.get_balance(&payer_keypair.pubkey()),
            payer_base_balance
        );
        assert_eq!(bank.get_balance(&buffer_address), 0);
        assert_eq!(None, bank.get_account(&buffer_address));
        let post_program_account = bank.get_account(&program_keypair.pubkey()).unwrap();
        assert_eq!(post_program_account.lamports(), min_program_balance);
        assert_eq!(post_program_account.owner(), &bpf_loader_upgradeable::id());
        assert_eq!(
            post_program_account.data().len(),
            UpgradeableLoaderState::program_len().unwrap()
        );
        let state: UpgradeableLoaderState = post_program_account.state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Program {
                programdata_address
            }
        );
        let post_programdata_account = bank.get_account(&programdata_address).unwrap();
        assert_eq!(post_programdata_account.lamports(), min_programdata_balance);
        assert_eq!(
            post_programdata_account.owner(),
            &bpf_loader_upgradeable::id()
        );
        let state: UpgradeableLoaderState = post_programdata_account.state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::ProgramData {
                slot: bank_client.get_slot().unwrap(),
                upgrade_authority_address: Some(upgrade_authority_keypair.pubkey())
            }
        );
        for (i, byte) in post_programdata_account.data()
            [UpgradeableLoaderState::programdata_data_offset().unwrap()..]
            .iter()
            .enumerate()
        {
            assert_eq!(elf[i], *byte);
        }

        // Test initialized program account
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        let message = Message::new(
            &[Instruction::new_with_bincode(
                bpf_loader_upgradeable::id(),
                &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                    max_data_len: elf.len(),
                },
                vec![
                    AccountMeta::new(mint_keypair.pubkey(), true),
                    AccountMeta::new(programdata_address, false),
                    AccountMeta::new(program_keypair.pubkey(), false),
                    AccountMeta::new(buffer_address, false),
                    AccountMeta::new_readonly(sysvar::rent::id(), false),
                    AccountMeta::new_readonly(sysvar::clock::id(), false),
                    AccountMeta::new_readonly(system_program::id(), false),
                    AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), true),
                ],
            )],
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(0, InstructionError::AccountAlreadyInitialized),
            bank_client
                .send_and_confirm_message(&[&mint_keypair, &upgrade_authority_keypair], message)
                .unwrap_err()
                .unwrap()
        );

        // Test initialized ProgramData account
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::Custom(0)),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test deploy no authority
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &program_account);
        bank.store_account(&programdata_address, &programdata_account);
        let message = Message::new(
            &[Instruction::new_with_bincode(
                bpf_loader_upgradeable::id(),
                &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                    max_data_len: elf.len(),
                },
                vec![
                    AccountMeta::new(mint_keypair.pubkey(), true),
                    AccountMeta::new(programdata_address, false),
                    AccountMeta::new(program_keypair.pubkey(), false),
                    AccountMeta::new(buffer_address, false),
                    AccountMeta::new_readonly(sysvar::rent::id(), false),
                    AccountMeta::new_readonly(sysvar::clock::id(), false),
                    AccountMeta::new_readonly(system_program::id(), false),
                ],
            )],
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(0, InstructionError::NotEnoughAccountKeys),
            bank_client
                .send_and_confirm_message(&[&mint_keypair], message)
                .unwrap_err()
                .unwrap()
        );

        // Test deploy authority not a signer
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &program_account);
        bank.store_account(&programdata_address, &programdata_account);
        let message = Message::new(
            &[Instruction::new_with_bincode(
                bpf_loader_upgradeable::id(),
                &UpgradeableLoaderInstruction::DeployWithMaxDataLen {
                    max_data_len: elf.len(),
                },
                vec![
                    AccountMeta::new(mint_keypair.pubkey(), true),
                    AccountMeta::new(programdata_address, false),
                    AccountMeta::new(program_keypair.pubkey(), false),
                    AccountMeta::new(buffer_address, false),
                    AccountMeta::new_readonly(sysvar::rent::id(), false),
                    AccountMeta::new_readonly(sysvar::clock::id(), false),
                    AccountMeta::new_readonly(system_program::id(), false),
                    AccountMeta::new_readonly(upgrade_authority_keypair.pubkey(), false),
                ],
            )],
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(0, InstructionError::MissingRequiredSignature),
            bank_client
                .send_and_confirm_message(&[&mint_keypair], message)
                .unwrap_err()
                .unwrap()
        );

        // Test invalid Buffer account state
        bank.clear_signatures();
        bank.store_account(&buffer_address, &AccountSharedData::default());
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test program account not rent exempt
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance - 1,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::ExecutableAccountNotRentExempt),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test program account not rent exempt because data is larger than needed
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let mut instructions = bpf_loader_upgradeable::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap();
        instructions[0] = system_instruction::create_account(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            min_program_balance,
            UpgradeableLoaderState::program_len().unwrap() as u64 + 1,
            &id(),
        );
        let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::ExecutableAccountNotRentExempt),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test program account too small
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let mut instructions = bpf_loader_upgradeable::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap();
        instructions[0] = system_instruction::create_account(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            min_program_balance,
            UpgradeableLoaderState::program_len().unwrap() as u64 - 1,
            &id(),
        );
        let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::AccountDataTooSmall),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test Insufficient payer funds (need more funds to cover the
        // difference between buffer lamports and programdata lamports)
        bank.clear_signatures();
        bank.store_account(
            &mint_keypair.pubkey(),
            &AccountSharedData::new(deploy_fees + min_program_balance, 0, &system_program::id()),
        );
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::Custom(1)),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );
        bank.store_account(
            &mint_keypair.pubkey(),
            &AccountSharedData::new(1_000_000_000, 0, &system_program::id()),
        );

        // Test max_data_len
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len() - 1,
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::AccountDataTooSmall),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test max_data_len too large
        bank.clear_signatures();
        bank.store_account(
            &mint_keypair.pubkey(),
            &AccountSharedData::new(u64::MAX / 2, 0, &system_program::id()),
        );
        let mut modified_buffer_account = buffer_account.clone();
        modified_buffer_account.set_lamports(u64::MAX / 2);
        bank.store_account(&buffer_address, &modified_buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                usize::MAX,
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::InvalidArgument),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test not the system account
        bank.clear_signatures();
        bank.store_account(&buffer_address, &buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let mut instructions = bpf_loader_upgradeable::deploy_with_max_program_len(
            &mint_keypair.pubkey(),
            &program_keypair.pubkey(),
            &buffer_address,
            &upgrade_authority_keypair.pubkey(),
            min_program_balance,
            elf.len(),
        )
        .unwrap();
        instructions[1].accounts[6] = AccountMeta::new_readonly(Pubkey::new_unique(), false);
        let message = Message::new(&instructions, Some(&mint_keypair.pubkey()));
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::MissingAccount),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test Bad ELF data
        bank.clear_signatures();
        let mut modified_buffer_account = buffer_account;
        truncate_data(
            &mut modified_buffer_account,
            UpgradeableLoaderState::buffer_len(1).unwrap(),
        );
        bank.store_account(&buffer_address, &modified_buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Test small buffer account
        bank.clear_signatures();
        let mut modified_buffer_account = AccountSharedData::new(
            min_programdata_balance,
            UpgradeableLoaderState::buffer_len(elf.len()).unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        modified_buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_keypair.pubkey()),
            })
            .unwrap();
        modified_buffer_account.data_as_mut_slice()
            [UpgradeableLoaderState::buffer_data_offset().unwrap()..]
            .copy_from_slice(&elf);
        truncate_data(&mut modified_buffer_account, 5);
        bank.store_account(&buffer_address, &modified_buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::InvalidAccountData),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Mismatched buffer and program authority
        bank.clear_signatures();
        let mut modified_buffer_account = AccountSharedData::new(
            min_programdata_balance,
            UpgradeableLoaderState::buffer_len(elf.len()).unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        modified_buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(buffer_address),
            })
            .unwrap();
        modified_buffer_account.data_as_mut_slice()
            [UpgradeableLoaderState::buffer_data_offset().unwrap()..]
            .copy_from_slice(&elf);
        bank.store_account(&buffer_address, &modified_buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::IncorrectAuthority),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );

        // Deploy buffer with mismatched None authority
        bank.clear_signatures();
        let mut modified_buffer_account = AccountSharedData::new(
            min_programdata_balance,
            UpgradeableLoaderState::buffer_len(elf.len()).unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        modified_buffer_account
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: None,
            })
            .unwrap();
        modified_buffer_account.data_as_mut_slice()
            [UpgradeableLoaderState::buffer_data_offset().unwrap()..]
            .copy_from_slice(&elf);
        bank.store_account(&buffer_address, &modified_buffer_account);
        bank.store_account(&program_keypair.pubkey(), &AccountSharedData::default());
        bank.store_account(&programdata_address, &AccountSharedData::default());
        let message = Message::new(
            &bpf_loader_upgradeable::deploy_with_max_program_len(
                &mint_keypair.pubkey(),
                &program_keypair.pubkey(),
                &buffer_address,
                &upgrade_authority_keypair.pubkey(),
                min_program_balance,
                elf.len(),
            )
            .unwrap(),
            Some(&mint_keypair.pubkey()),
        );
        assert_eq!(
            TransactionError::InstructionError(1, InstructionError::IncorrectAuthority),
            bank_client
                .send_and_confirm_message(
                    &[&mint_keypair, &program_keypair, &upgrade_authority_keypair],
                    message
                )
                .unwrap_err()
                .unwrap()
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_upgrade() {
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Upgrade).unwrap();
        let loader_id = bpf_loader_upgradeable::id();
        let mut file = File::open("test_elfs/noop_aligned.so").expect("file open failed");
        let mut elf_orig = Vec::new();
        file.read_to_end(&mut elf_orig).unwrap();
        let mut file = File::open("test_elfs/noop_unaligned.so").expect("file open failed");
        let mut elf_new = Vec::new();
        file.read_to_end(&mut elf_new).unwrap();
        assert_ne!(elf_orig.len(), elf_new.len());
        let rent = Rent::default();
        let rent_account = RefCell::new(create_account_for_test(&Rent::default()));
        let slot = 42;
        let clock_account = RefCell::new(create_account_for_test(&Clock {
            slot,
            ..Clock::default()
        }));
        let min_program_balance =
            1.max(rent.minimum_balance(UpgradeableLoaderState::program_len().unwrap()));
        let min_programdata_balance = 1.max(rent.minimum_balance(
            UpgradeableLoaderState::programdata_len(elf_orig.len().max(elf_new.len())).unwrap(),
        ));
        let upgrade_authority_address = Pubkey::new_unique();
        let buffer_address = Pubkey::new_unique();
        let program_address = Pubkey::new_unique();
        let (programdata_address, _) =
            Pubkey::find_program_address(&[program_address.as_ref()], &loader_id);
        let spill_address = Pubkey::new_unique();
        let upgrade_authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let rent_id = sysvar::rent::id();
        let clock_id = sysvar::clock::id();

        #[allow(clippy::type_complexity)]
        fn get_accounts(
            buffer_authority: &Pubkey,
            programdata_address: &Pubkey,
            upgrade_authority_address: &Pubkey,
            slot: u64,
            elf_orig: &[u8],
            elf_new: &[u8],
            min_program_balance: u64,
            min_programdata_balance: u64,
        ) -> (
            Rc<RefCell<AccountSharedData>>,
            Rc<RefCell<AccountSharedData>>,
            Rc<RefCell<AccountSharedData>>,
            Rc<RefCell<AccountSharedData>>,
        ) {
            let buffer_account = AccountSharedData::new_ref(
                1,
                UpgradeableLoaderState::buffer_len(elf_new.len()).unwrap(),
                &bpf_loader_upgradeable::id(),
            );
            buffer_account
                .borrow_mut()
                .set_state(&UpgradeableLoaderState::Buffer {
                    authority_address: Some(*buffer_authority),
                })
                .unwrap();
            buffer_account.borrow_mut().data_as_mut_slice()
                [UpgradeableLoaderState::buffer_data_offset().unwrap()..]
                .copy_from_slice(elf_new);
            let programdata_account = AccountSharedData::new_ref(
                min_programdata_balance,
                UpgradeableLoaderState::programdata_len(elf_orig.len().max(elf_new.len())).unwrap(),
                &bpf_loader_upgradeable::id(),
            );
            programdata_account
                .borrow_mut()
                .set_state(&UpgradeableLoaderState::ProgramData {
                    slot,
                    upgrade_authority_address: Some(*upgrade_authority_address),
                })
                .unwrap();
            let program_account = AccountSharedData::new_ref(
                min_program_balance,
                UpgradeableLoaderState::program_len().unwrap(),
                &bpf_loader_upgradeable::id(),
            );
            program_account.borrow_mut().set_executable(true);
            program_account
                .borrow_mut()
                .set_state(&UpgradeableLoaderState::Program {
                    programdata_address: *programdata_address,
                })
                .unwrap();
            let spill_account = AccountSharedData::new_ref(0, 0, &Pubkey::new_unique());

            (
                buffer_account,
                program_account,
                programdata_account,
                spill_account,
            )
        }

        // Case: Success
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        assert_eq!(0, buffer_account.borrow().lamports());
        assert_eq!(
            min_programdata_balance,
            programdata_account.borrow().lamports()
        );
        assert_eq!(1, spill_account.borrow().lamports());
        let state: UpgradeableLoaderState = programdata_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(upgrade_authority_address)
            }
        );
        for (i, byte) in programdata_account.borrow().data()
            [UpgradeableLoaderState::programdata_data_offset().unwrap()
                ..UpgradeableLoaderState::programdata_data_offset().unwrap() + elf_new.len()]
            .iter()
            .enumerate()
        {
            assert_eq!(elf_new[i], *byte);
        }

        // Case: not upgradable
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None,
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::Immutable),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: wrong authority
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let invalid_upgrade_authority_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &invalid_upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: authority did not sign
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                false,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Program account not executable
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        program_account.borrow_mut().set_executable(false);
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::AccountNotExecutable),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Program account now owned by loader
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        program_account.borrow_mut().set_owner(Pubkey::new_unique());
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectProgramId),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Program account not writable
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, false, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidArgument),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Program account not initialized
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        program_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Uninitialized)
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidAccountData),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Program ProgramData account mismatch
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let invalid_programdata_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (
                false,
                true,
                &invalid_programdata_address,
                &programdata_account,
            ),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidArgument),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Buffer account not initialized
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Uninitialized)
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidArgument),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Buffer account too big
        let (_, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(elf_orig.len().max(elf_new.len()) + 1).unwrap(),
            &loader_id,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::AccountDataTooSmall),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Test small buffer account
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &upgrade_authority_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        truncate_data(&mut buffer_account.borrow_mut(), 5);
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidAccountData),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: Mismatched buffer and program authority
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &buffer_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: None buffer authority
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &buffer_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: None,
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: None buffer and program authority
        let (buffer_account, program_account, programdata_account, spill_account) = get_accounts(
            &buffer_address,
            &programdata_address,
            &upgrade_authority_address,
            slot,
            &elf_orig,
            &elf_new,
            min_program_balance,
            min_programdata_balance,
        );
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: None,
            })
            .unwrap();
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None,
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, true, &programdata_address, &programdata_account),
            (false, true, &program_address, &program_account),
            (false, true, &buffer_address, &buffer_account),
            (false, true, &spill_address, &spill_account),
            (false, false, &rent_id, &rent_account),
            (false, false, &clock_id, &clock_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_set_upgrade_authority() {
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap();
        let loader_id = bpf_loader_upgradeable::id();
        let slot = 0;
        let upgrade_authority_address = Pubkey::new_unique();
        let upgrade_authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let new_upgrade_authority_address = Pubkey::new_unique();
        let new_upgrade_authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let program_address = Pubkey::new_unique();
        let (programdata_address, _) = Pubkey::find_program_address(
            &[program_address.as_ref()],
            &bpf_loader_upgradeable::id(),
        );
        let programdata_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::programdata_len(0).unwrap(),
            &bpf_loader_upgradeable::id(),
        );

        // Case: Set to new authority
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
            (
                true,
                false,
                &new_upgrade_authority_address,
                &new_upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = programdata_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(new_upgrade_authority_address),
            }
        );

        // Case: Not upgradeable
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                true,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = programdata_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None,
            }
        );

        // Case: Authority did not sign
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                false,
                false,
                &upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: wrong authority
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: Some(upgrade_authority_address),
            })
            .unwrap();
        let invalid_upgrade_authority_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                true,
                false,
                &invalid_upgrade_authority_address,
                &upgrade_authority_account,
            ),
            (
                false,
                false,
                &new_upgrade_authority_address,
                &new_upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: No authority
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot,
                upgrade_authority_address: None,
            })
            .unwrap();
        let invalid_upgrade_authority_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                true,
                false,
                &invalid_upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::Immutable),
            process_instruction(
                &loader_id,
                &bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap(),
                &keyed_accounts,
            )
        );

        // Case: Not a ProgramData account
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address: Pubkey::new_unique(),
            })
            .unwrap();
        let invalid_upgrade_authority_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (
                true,
                false,
                &invalid_upgrade_authority_address,
                &upgrade_authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::InvalidArgument),
            process_instruction(
                &loader_id,
                &bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap(),
                &keyed_accounts,
            )
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_set_buffer_authority() {
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap();
        let loader_id = bpf_loader_upgradeable::id();
        let authority_address = Pubkey::new_unique();
        let authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let new_authority_address = Pubkey::new_unique();
        let new_authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let buffer_address = Pubkey::new_unique();
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(0).unwrap(),
            &loader_id,
        );

        // Case: New authority required
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        let mut keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (true, false, &authority_address, &authority_account),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            }
        );

        // Case: Set to new authority
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        keyed_accounts.push((false, false, &new_authority_address, &new_authority_account));
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(
            state,
            UpgradeableLoaderState::Buffer {
                authority_address: Some(new_authority_address),
            }
        );

        // Case: Authority did not sign
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        keyed_accounts[1] = (false, false, &authority_address, &authority_account);
        assert_eq!(
            Err(InstructionError::MissingRequiredSignature),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: wrong authority
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        let invalid_authority_address = Pubkey::new_unique();
        keyed_accounts[1] = (true, false, &invalid_authority_address, &authority_account);
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: No authority
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: None,
            })
            .unwrap();
        assert_eq!(
            Err(InstructionError::Immutable),
            process_instruction(
                &loader_id,
                &bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap(),
                &keyed_accounts,
            )
        );

        // Case: Not a Buffer account
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address: Pubkey::new_unique(),
            })
            .unwrap();
        keyed_accounts.pop();
        assert_eq!(
            Err(InstructionError::InvalidArgument),
            process_instruction(
                &loader_id,
                &bincode::serialize(&UpgradeableLoaderInstruction::SetAuthority).unwrap(),
                &keyed_accounts,
            )
        );

        // Case: Set to no authority
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        keyed_accounts[1] = (true, false, &authority_address, &authority_account);
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
    }

    #[test]
    fn test_bpf_loader_upgradeable_close() {
        let instruction = bincode::serialize(&UpgradeableLoaderInstruction::Close).unwrap();
        let loader_id = bpf_loader_upgradeable::id();
        let authority_address = Pubkey::new_unique();
        let authority_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let recipient_address = Pubkey::new_unique();
        let recipient_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let buffer_address = Pubkey::new_unique();
        let buffer_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::buffer_len(0).unwrap(),
            &bpf_loader_upgradeable::id(),
        );

        // Case: close a buffer account
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (false, false, &recipient_address, &recipient_account),
            (true, false, &authority_address, &authority_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        assert_eq!(0, buffer_account.borrow().lamports());
        assert_eq!(2, recipient_account.borrow().lamports());
        let state: UpgradeableLoaderState = buffer_account.borrow().state().unwrap();
        assert_eq!(state, UpgradeableLoaderState::Uninitialized);

        // Case: close with wrong authority
        buffer_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Buffer {
                authority_address: Some(authority_address),
            })
            .unwrap();
        let incorrect_authority_address = Pubkey::new_unique();
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &buffer_address, &buffer_account),
            (false, false, &recipient_address, &recipient_account),
            (
                true,
                false,
                &incorrect_authority_address,
                &authority_account,
            ),
        ];
        assert_eq!(
            Err(InstructionError::IncorrectAuthority),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );

        // Case: close an uninitialized account
        let uninitialized_address = Pubkey::new_unique();
        let uninitialized_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::programdata_len(0).unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        uninitialized_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Uninitialized)
            .unwrap();
        let recipient_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &uninitialized_address, &uninitialized_account),
            (false, false, &recipient_address, &recipient_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        assert_eq!(0, uninitialized_account.borrow().lamports());
        assert_eq!(2, recipient_account.borrow().lamports());
        let state: UpgradeableLoaderState = uninitialized_account.borrow().state().unwrap();
        assert_eq!(state, UpgradeableLoaderState::Uninitialized);

        // Case: close a program account
        let programdata_address = Pubkey::new_unique();
        let programdata_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::programdata_len(0).unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        programdata_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::ProgramData {
                slot: 0,
                upgrade_authority_address: Some(authority_address),
            })
            .unwrap();
        let program_address = Pubkey::new_unique();
        let program_account = AccountSharedData::new_ref(
            1,
            UpgradeableLoaderState::program_len().unwrap(),
            &bpf_loader_upgradeable::id(),
        );
        program_account.borrow_mut().set_executable(true);
        program_account
            .borrow_mut()
            .set_state(&UpgradeableLoaderState::Program {
                programdata_address,
            })
            .unwrap();
        let recipient_account = AccountSharedData::new_ref(1, 0, &Pubkey::new_unique());
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (false, false, &recipient_address, &recipient_account),
            (true, false, &authority_address, &authority_account),
            (false, true, &program_address, &program_account),
        ];
        assert_eq!(
            Ok(()),
            process_instruction(&loader_id, &instruction, &keyed_accounts),
        );
        assert_eq!(0, programdata_account.borrow().lamports());
        assert_eq!(2, recipient_account.borrow().lamports());
        let state: UpgradeableLoaderState = programdata_account.borrow().state().unwrap();
        assert_eq!(state, UpgradeableLoaderState::Uninitialized);

        // Try to invoke closed account
        let keyed_accounts: Vec<KeyedAccountTuple> = vec![
            (false, false, &programdata_address, &programdata_account),
            (false, false, &program_address, &program_account),
        ];
        assert_eq!(
            Err(InstructionError::InvalidAccountData),
            process_instruction(&program_address, &instruction, &keyed_accounts),
        );
    }

    /// fuzzing utility function
    fn fuzz<F>(
        bytes: &[u8],
        outer_iters: usize,
        inner_iters: usize,
        offset: Range<usize>,
        value: Range<u8>,
        work: F,
    ) where
        F: Fn(&mut [u8]),
    {
        let mut rng = rand::thread_rng();
        for _ in 0..outer_iters {
            let mut mangled_bytes = bytes.to_vec();
            for _ in 0..inner_iters {
                let offset = rng.gen_range(offset.start, offset.end);
                let value = rng.gen_range(value.start, value.end);
                mangled_bytes[offset] = value;
                work(&mut mangled_bytes);
            }
        }
    }

    #[test]
    #[ignore]
    fn test_fuzz() {
        let loader_id = solana_sdk::pubkey::new_rand();
        let program_key = solana_sdk::pubkey::new_rand();

        // Create program account
        let mut file = File::open("test_elfs/noop_aligned.so").expect("file open failed");
        let mut elf = Vec::new();
        file.read_to_end(&mut elf).unwrap();

        // Mangle the whole file
        fuzz(
            &elf,
            1_000_000_000,
            100,
            0..elf.len(),
            0..255,
            |bytes: &mut [u8]| {
                let program_account = AccountSharedData::new_ref(1, 0, &loader_id);
                program_account.borrow_mut().set_data(bytes.to_vec());
                program_account.borrow_mut().set_executable(true);

                let parameter_account = AccountSharedData::new_ref(1, 0, &loader_id);
                let keyed_accounts: Vec<KeyedAccountTuple> = vec![
                    (false, false, &program_key, &program_account),
                    (false, false, &program_key, &parameter_account),
                ];

                let _result = process_instruction(&bpf_loader::id(), &[], &keyed_accounts);
            },
        );
    }
}
