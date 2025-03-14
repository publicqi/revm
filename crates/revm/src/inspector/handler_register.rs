use core::cell::RefCell;

use crate::{
    db::Database,
    handler::register::{EvmHandler, EvmInstructionTables},
    interpreter::{opcode, opcode::BoxedInstruction, InstructionResult, Interpreter},
    Evm, FrameOrResult, FrameResult, Inspector, JournalEntry,
};
use alloc::{boxed::Box, rc::Rc, sync::Arc, vec::Vec};

/// Provides access to an `Inspector` instance.
pub trait GetInspector<DB: Database> {
    fn get_inspector(&mut self) -> &mut dyn Inspector<DB>;
}

impl<DB: Database, INSP: Inspector<DB>> GetInspector<DB> for INSP {
    fn get_inspector(&mut self) -> &mut dyn Inspector<DB> {
        self
    }
}

/// Register Inspector handles that interact with Inspector instance.
///
///
/// # Note
///
/// Inspector handle register does not override any existing handlers, and it
/// calls them before (or after) calling Inspector. This means that it is safe
/// to use this register with any other register.
///
/// A few instructions handlers are wrapped twice once for `step` and `step_end`
/// and in case of Logs and Selfdestruct wrapper is wrapped again for the
/// `log` and `selfdestruct` calls.
pub fn inspector_handle_register<'a, DB: Database, EXT: GetInspector<DB>>(
    handler: &mut EvmHandler<'a, EXT, DB>,
) {
    // Every instruction inside flat table that is going to be wrapped by inspector calls.
    let table = handler
        .instruction_table
        .take()
        .expect("Handler must have instruction table");
    let mut table = match table {
        EvmInstructionTables::Plain(table) => table
            .into_iter()
            .map(|i| inspector_instruction(i))
            .collect::<Vec<_>>(),
        EvmInstructionTables::Boxed(table) => table
            .into_iter()
            .map(|i| inspector_instruction(i))
            .collect::<Vec<_>>(),
    };

    // Register inspector Log instruction.
    let mut inspect_log = |index: u8| {
        if let Some(i) = table.get_mut(index as usize) {
            let old = core::mem::replace(i, Box::new(|_, _| ()));
            *i = Box::new(
                move |interpreter: &mut Interpreter, host: &mut Evm<'a, EXT, DB>| {
                    let old_log_len = host.context.evm.journaled_state.logs.len();
                    old(interpreter, host);
                    // check if log was added. It is possible that revert happened
                    // cause of gas or stack underflow.
                    if host.context.evm.journaled_state.logs.len() == old_log_len + 1 {
                        // clone log.
                        // TODO decide if we should remove this and leave the comment
                        // that log can be found as journaled_state.
                        let last_log = host
                            .context
                            .evm
                            .journaled_state
                            .logs
                            .last()
                            .unwrap()
                            .clone();
                        // call Inspector
                        host.context
                            .external
                            .get_inspector()
                            .log(&mut host.context.evm, &last_log);
                    }
                },
            )
        }
    };

    inspect_log(opcode::LOG0);
    inspect_log(opcode::LOG1);
    inspect_log(opcode::LOG2);
    inspect_log(opcode::LOG3);
    inspect_log(opcode::LOG4);

    // // register selfdestruct function.
    if let Some(i) = table.get_mut(opcode::SELFDESTRUCT as usize) {
        let old = core::mem::replace(i, Box::new(|_, _| ()));
        *i = Box::new(
            move |interpreter: &mut Interpreter, host: &mut Evm<'a, EXT, DB>| {
                // execute selfdestruct
                old(interpreter, host);
                // check if selfdestruct was successful and if journal entry is made.
                if let Some(JournalEntry::AccountDestroyed {
                    address,
                    target,
                    had_balance,
                    ..
                }) = host
                    .context
                    .evm
                    .journaled_state
                    .journal
                    .last()
                    .unwrap()
                    .last()
                {
                    host.context.external.get_inspector().selfdestruct(
                        *address,
                        *target,
                        *had_balance,
                    );
                }
            },
        )
    }

    // cast vector to array.
    handler.instruction_table = Some(EvmInstructionTables::Boxed(
        table.try_into().unwrap_or_else(|_| unreachable!()),
    ));

    // call and create input stack shared between handlers. They are used to share
    // inputs in *_end Inspector calls.
    let call_input_stack = Rc::<RefCell<Vec<_>>>::new(RefCell::new(Vec::new()));
    let create_input_stack = Rc::<RefCell<Vec<_>>>::new(RefCell::new(Vec::new()));

    // Create handle
    let create_input_stack_inner = create_input_stack.clone();
    let old_handle = handler.execution.create.clone();
    handler.execution.create = Arc::new(move |ctx, mut inputs| -> FrameOrResult {
        let inspector = ctx.external.get_inspector();
        // call inspector create to change input or return outcome.
        if let Some(outcome) = inspector.create(&mut ctx.evm, &mut inputs) {
            create_input_stack_inner.borrow_mut().push(inputs.clone());
            return FrameOrResult::Result(FrameResult::Create(outcome));
        }
        create_input_stack_inner.borrow_mut().push(inputs.clone());

        let mut frame_or_result = old_handle(ctx, inputs);

        let inspector = ctx.external.get_inspector();
        if let FrameOrResult::Frame(frame) = &mut frame_or_result {
            inspector.initialize_interp(&mut frame.frame_data_mut().interpreter, &mut ctx.evm)
        }
        frame_or_result
    });

    // Call handler
    let call_input_stack_inner = call_input_stack.clone();
    let old_handle = handler.execution.call.clone();
    handler.execution.call = Arc::new(move |ctx, mut inputs| -> FrameOrResult {
        let inspector = ctx.external.get_inspector();
        let _mems = inputs.return_memory_offset.clone();
        // call inspector callto change input or return outcome.
        if let Some(outcome) = inspector.call(&mut ctx.evm, &mut inputs) {
            call_input_stack_inner.borrow_mut().push(inputs.clone());
            return FrameOrResult::Result(FrameResult::Call(outcome));
        }
        call_input_stack_inner.borrow_mut().push(inputs.clone());

        let mut frame_or_result = old_handle(ctx, inputs);

        let inspector = ctx.external.get_inspector();
        if let FrameOrResult::Frame(frame) = &mut frame_or_result {
            inspector.initialize_interp(&mut frame.frame_data_mut().interpreter, &mut ctx.evm)
        }
        frame_or_result
    });

    // call outcome
    let call_input_stack_inner = call_input_stack.clone();
    let old_handle = handler.execution.insert_call_outcome.clone();
    handler.execution.insert_call_outcome =
        Arc::new(move |ctx, frame, shared_memory, mut outcome| {
            let inspector = ctx.external.get_inspector();
            let call_inputs = call_input_stack_inner.borrow_mut().pop().unwrap();
            outcome = inspector.call_end(&mut ctx.evm, &call_inputs, outcome);
            old_handle(ctx, frame, shared_memory, outcome)
        });

    // create outcome
    let create_input_stack_inner = create_input_stack.clone();
    let old_handle = handler.execution.insert_create_outcome.clone();
    handler.execution.insert_create_outcome = Arc::new(move |ctx, frame, mut outcome| {
        let inspector = ctx.external.get_inspector();
        let create_inputs = create_input_stack_inner.borrow_mut().pop().unwrap();
        outcome = inspector.create_end(&mut ctx.evm, &create_inputs, outcome);
        old_handle(ctx, frame, outcome)
    });

    // last frame outcome
    let old_handle = handler.execution.last_frame_return.clone();
    handler.execution.last_frame_return = Arc::new(move |ctx, frame_result| {
        let inspector = ctx.external.get_inspector();
        match frame_result {
            FrameResult::Call(outcome) => {
                let call_inputs = call_input_stack.borrow_mut().pop().unwrap();
                *outcome = inspector.call_end(&mut ctx.evm, &call_inputs, outcome.clone());
            }
            FrameResult::Create(outcome) => {
                let create_inputs = create_input_stack.borrow_mut().pop().unwrap();
                *outcome = inspector.create_end(&mut ctx.evm, &create_inputs, outcome.clone());
            }
        }
        //inspector.last_frame_return(ctx, frame_result);
        old_handle(ctx, frame_result)
    });
}

/// Outer closure that calls Inspector for every instruction.
pub fn inspector_instruction<
    'a,
    INSP: GetInspector<DB>,
    DB: Database,
    Instruction: Fn(&mut Interpreter, &mut Evm<'a, INSP, DB>) + 'a,
>(
    instruction: Instruction,
) -> BoxedInstruction<'a, Evm<'a, INSP, DB>> {
    Box::new(
        move |interpreter: &mut Interpreter, host: &mut Evm<'a, INSP, DB>| {
            // SAFETY: as the PC was already incremented we need to subtract 1 to preserve the
            // old Inspector behavior.
            interpreter.instruction_pointer = unsafe { interpreter.instruction_pointer.sub(1) };

            host.context
                .external
                .get_inspector()
                .step(interpreter, &mut host.context.evm);
            if interpreter.instruction_result != InstructionResult::Continue {
                return;
            }

            // return PC to old value
            interpreter.instruction_pointer = unsafe { interpreter.instruction_pointer.add(1) };

            // execute instruction.
            instruction(interpreter, host);

            host.context
                .external
                .get_inspector()
                .step_end(interpreter, &mut host.context.evm);
        },
    )
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::{
        db::EmptyDB,
        inspectors::NoOpInspector,
        interpreter::{opcode::*, CallInputs, CreateInputs, Interpreter},
        primitives::BerlinSpec,
        Database, Evm, EvmContext, Inspector,
    };

    use revm_interpreter::{CallOutcome, CreateOutcome};

    #[test]
    fn test_make_boxed_instruction_table() {
        // test that this pattern builds.
        let inst: InstructionTable<Evm<'_, NoOpInspector, EmptyDB>> =
            make_instruction_table::<Evm<'_, _, _>, BerlinSpec>();
        let _test: BoxedInstructionTable<'_, Evm<'_, _, _>> =
            make_boxed_instruction_table::<'_, Evm<'_, NoOpInspector, EmptyDB>, BerlinSpec, _>(
                inst,
                inspector_instruction,
            );
    }

    #[derive(Default, Debug)]
    struct StackInspector {
        initialize_interp_called: bool,
        step: u32,
        step_end: u32,
        call: bool,
        call_end: bool,
    }

    impl<DB: Database> Inspector<DB> for StackInspector {
        fn initialize_interp(&mut self, _interp: &mut Interpreter, _context: &mut EvmContext<DB>) {
            if self.initialize_interp_called {
                unreachable!("initialize_interp should not be called twice")
            }
            self.initialize_interp_called = true;
        }

        fn step(&mut self, _interp: &mut Interpreter, _context: &mut EvmContext<DB>) {
            self.step += 1;
        }

        fn step_end(&mut self, _interp: &mut Interpreter, _context: &mut EvmContext<DB>) {
            self.step_end += 1;
        }

        fn call(
            &mut self,
            context: &mut EvmContext<DB>,
            _call: &mut CallInputs,
        ) -> Option<CallOutcome> {
            if self.call {
                unreachable!("call should not be called twice")
            }
            self.call = true;
            assert_eq!(context.journaled_state.depth(), 0);
            None
        }

        fn call_end(
            &mut self,
            context: &mut EvmContext<DB>,
            _inputs: &CallInputs,
            outcome: CallOutcome,
        ) -> CallOutcome {
            if self.call_end {
                unreachable!("call_end should not be called twice")
            }
            assert_eq!(context.journaled_state.depth(), 0);
            self.call_end = true;
            outcome
        }

        fn create(
            &mut self,
            context: &mut EvmContext<DB>,
            _call: &mut CreateInputs,
        ) -> Option<CreateOutcome> {
            assert_eq!(context.journaled_state.depth(), 0);
            None
        }

        fn create_end(
            &mut self,
            context: &mut EvmContext<DB>,
            _inputs: &CreateInputs,
            outcome: CreateOutcome,
        ) -> CreateOutcome {
            assert_eq!(context.journaled_state.depth(), 0);
            outcome
        }
    }

    #[test]
    fn test_inspector_handlers() {
        use crate::{
            db::BenchmarkDB,
            inspector::inspector_handle_register,
            interpreter::opcode,
            primitives::{address, Bytecode, Bytes, TransactTo},
            Evm,
        };

        let contract_data: Bytes = Bytes::from(vec![
            opcode::PUSH1,
            0x1,
            opcode::PUSH1,
            0xb,
            opcode::PUSH1,
            0x1,
            opcode::PUSH1,
            0x1,
            opcode::PUSH1,
            0x1,
            opcode::CREATE,
            opcode::STOP,
        ]);
        let bytecode = Bytecode::new_raw(contract_data);

        let mut evm: Evm<'_, StackInspector, BenchmarkDB> = Evm::builder()
            .with_db(BenchmarkDB::new_bytecode(bytecode.clone()))
            .with_external_context(StackInspector::default())
            .modify_tx_env(|tx| {
                tx.clear();
                tx.caller = address!("1000000000000000000000000000000000000000");
                tx.transact_to =
                    TransactTo::Call(address!("0000000000000000000000000000000000000000"));
                tx.gas_limit = 21100;
            })
            .append_handler_register(inspector_handle_register)
            .build();

        // run evm.
        evm.transact().unwrap();

        let inspector = evm.into_context().external;

        assert_eq!(inspector.step, 6);
        assert_eq!(inspector.step_end, 6);
        assert!(inspector.initialize_interp_called);
        assert!(inspector.call);
        assert!(inspector.call_end);
    }

    #[test]
    fn test_inspector_reg() {
        let mut noop = NoOpInspector;
        let _evm = Evm::builder()
            .with_external_context(&mut noop)
            .append_handler_register(inspector_handle_register)
            .build();
    }
}
