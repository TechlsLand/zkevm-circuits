use crate::{
    evm_circuit::{
        execution::ExecutionGadget,
        param::{N_BYTES_ACCOUNT_ADDRESS, N_BYTES_GAS, N_BYTES_MEMORY_WORD_SIZE, N_BYTES_U64},
        step::ExecutionState,
        util::{
            common_gadget::TransferGadget,
            constraint_builder::{
                ConstraintBuilder, ReversionInfo, StepStateTransition,
                Transition::{Any, Delta, To},
            },
            from_bytes,
            math_gadget::ConstantDivisionGadget,
            memory_gadget::{MemoryAddressGadget, MemoryExpansionGadget},
            not, select, CachedRegion, Cell, RandomLinearCombination, Word,
        },
        witness::{Block, Call, ExecStep, Transaction},
    },
    table::{AccountFieldTag, CallContextFieldTag},
    util::Expr,
};
use bus_mapping::{circuit_input_builder::CopyDataType, evm::OpcodeId};
use eth_types::{evm_types::GasCost, Field, ToLittleEndian, ToScalar, U256};
use ethers_core::utils::keccak256;
use halo2_proofs::{circuit::Value, plonk::Error};
use keccak256::EMPTY_HASH_LE;

/// Gadget for CREATE and CREATE2 opcodes
#[derive(Clone, Debug)]
pub(crate) struct CreateGadget<F> {
    opcode: Cell<F>,
    is_create2: Cell<F>,

    value: Word<F>,
    salt: Word<F>,
    new_address: RandomLinearCombination<F, N_BYTES_ACCOUNT_ADDRESS>,

    // tx access list for new address
    tx_id: Cell<F>,
    reversion_info: ReversionInfo<F>,
    was_warm: Cell<F>,

    caller_address: RandomLinearCombination<F, N_BYTES_ACCOUNT_ADDRESS>,
    nonce: RandomLinearCombination<F, N_BYTES_U64>,

    callee_reversion_info: ReversionInfo<F>,
    callee_is_success: Cell<F>,

    transfer: TransferGadget<F>,

    initialization_code: MemoryAddressGadget<F>,
    memory_expansion: MemoryExpansionGadget<F, 1, N_BYTES_MEMORY_WORD_SIZE>,

    gas_left: ConstantDivisionGadget<F, N_BYTES_GAS>,

    code_hash: Cell<F>,
}

impl<F: Field> ExecutionGadget<F> for CreateGadget<F> {
    const NAME: &'static str = "CREATE";

    const EXECUTION_STATE: ExecutionState = ExecutionState::CREATE;

    fn configure(cb: &mut ConstraintBuilder<F>) -> Self {
        // Use rw_counter of the step which triggers next call as its call_id.
        let callee_call_id = cb.curr.state.rw_counter.clone();

        let opcode = cb.query_cell();
        cb.opcode_lookup(opcode.expr(), 1.expr());

        let is_create2 = cb.query_bool();
        cb.require_equal(
            "Opcode is CREATE or CREATE2",
            opcode.expr(),
            select::expr(
                is_create2.expr(),
                OpcodeId::CREATE2.expr(),
                OpcodeId::CREATE.expr(),
            ),
        );

        let value = cb.query_word();
        cb.stack_pop(value.expr());

        let initialization_code = MemoryAddressGadget::construct_2(cb);
        cb.stack_pop(initialization_code.offset());
        cb.stack_pop(initialization_code.length());

        let salt = cb.condition(is_create2.expr(), |cb| {
            let salt = cb.query_word();
            cb.stack_pop(salt.expr());
            salt
        });
        let new_address = cb.query_rlc();
        let callee_is_success = cb.query_bool();
        cb.stack_push(callee_is_success.expr() * new_address.expr());

        let code_hash = cb.query_cell();
        cb.condition(initialization_code.has_length(), |cb| {
            cb.copy_table_lookup(
                cb.curr.state.call_id.expr(),
                CopyDataType::Memory.expr(),
                code_hash.expr(),
                CopyDataType::Bytecode.expr(),
                initialization_code.offset(),
                initialization_code.address(),
                0.expr(),
                initialization_code.length(),
                0.expr(),
                initialization_code.length(),
            );
        });
        cb.condition(not::expr(initialization_code.has_length()), |cb| {
            cb.require_equal(
                "",
                code_hash.expr(),
                Word::random_linear_combine_expr(
                    (*EMPTY_HASH_LE).map(|b| b.expr()),
                    cb.power_of_randomness(),
                ),
            );
        });

        let tx_id = cb.call_context(None, CallContextFieldTag::TxId);
        let mut reversion_info = cb.reversion_info_read(None);
        let was_warm = cb.query_bool();
        cb.account_access_list_write(
            tx_id.expr(),
            from_bytes::expr(&new_address.cells),
            1.expr(),
            was_warm.expr(),
            Some(&mut reversion_info),
        );

        let caller_address = cb.query_rlc();
        cb.call_context_lookup(
            0.expr(),
            None,
            CallContextFieldTag::CalleeAddress,
            from_bytes::expr(&caller_address.cells),
        );

        let nonce = cb.query_rlc();
        cb.account_write(
            caller_address.expr(),
            AccountFieldTag::Nonce,
            from_bytes::expr(&nonce.cells) + 1.expr(),
            from_bytes::expr(&nonce.cells),
            Some(&mut reversion_info),
        );

        let mut callee_reversion_info = cb.reversion_info_write(Some(callee_call_id.expr()));
        cb.account_write(
            new_address.expr(),
            AccountFieldTag::Nonce,
            1.expr(),
            0.expr(),
            Some(&mut callee_reversion_info),
        );

        let transfer = TransferGadget::construct(
            cb,
            caller_address.expr(),
            new_address.expr(),
            value.clone(),
            &mut callee_reversion_info,
        );

        let memory_expansion =
            MemoryExpansionGadget::construct(cb, [initialization_code.address()]);

        let stack_pointer_delta = 2.expr() + is_create2.expr();

        // EIP-150: all but one 64th of the caller's gas is sent to the callee.
        // let caller_gas_left =
        // (geth_step.gas.0 - geth_step.gas_cost.0 - memory_expansion_gas_cost) / 64;

        let gas_cost = GasCost::CREATE.expr() + memory_expansion.gas_cost();
        let gas_remaining = cb.curr.state.gas_left.expr() - gas_cost;
        let gas_left = ConstantDivisionGadget::construct(cb, gas_remaining.clone(), 64);
        let callee_gas_left = gas_remaining - gas_left.quotient();
        for (field_tag, value) in [
            (
                CallContextFieldTag::ProgramCounter,
                cb.curr.state.program_counter.expr() + 1.expr(),
            ),
            (
                CallContextFieldTag::StackPointer,
                cb.curr.state.stack_pointer.expr() + stack_pointer_delta,
            ),
            (CallContextFieldTag::GasLeft, gas_left.quotient()),
            (
                CallContextFieldTag::MemorySize,
                memory_expansion.next_memory_word_size(),
            ),
            (
                CallContextFieldTag::ReversibleWriteCounter,
                cb.curr.state.reversible_write_counter.expr() + 2.expr(),
            ),
        ] {
            cb.call_context_lookup(true.expr(), None, field_tag, value);
        }

        for (field_tag, value) in [
            (CallContextFieldTag::CallerId, cb.curr.state.call_id.expr()),
            (CallContextFieldTag::IsSuccess, callee_is_success.expr()),
            (
                CallContextFieldTag::IsPersistent,
                callee_reversion_info.is_persistent(),
            ),
            (CallContextFieldTag::TxId, tx_id.expr()),
            (CallContextFieldTag::CallerAddress, caller_address.expr()),
            (CallContextFieldTag::CalleeAddress, new_address.expr()),
            (
                CallContextFieldTag::RwCounterEndOfReversion,
                callee_reversion_info.rw_counter_end_of_reversion(),
            ),
        ] {
            cb.call_context_lookup(true.expr(), Some(callee_call_id.expr()), field_tag, value);
        }

        cb.require_step_state_transition(StepStateTransition {
            rw_counter: Delta(cb.rw_counter_offset()),
            call_id: To(callee_call_id.expr()),
            is_root: To(false.expr()),
            is_create: To(true.expr()),
            code_hash: To(code_hash.expr()),
            gas_left: To(callee_gas_left),
            reversible_write_counter: To(3.expr()),
            ..StepStateTransition::new_context()
        });

        Self {
            opcode,
            is_create2,
            reversion_info,
            tx_id,
            was_warm,
            value,
            salt,
            new_address,
            caller_address,
            nonce,
            callee_reversion_info,
            transfer,
            initialization_code,
            memory_expansion,
            gas_left,
            callee_is_success,
            code_hash,
        }
    }

    fn assign_exec_step(
        &self,
        region: &mut CachedRegion<'_, '_, F>,
        offset: usize,
        block: &Block<F>,
        tx: &Transaction,
        call: &Call,
        step: &ExecStep,
    ) -> Result<(), Error> {
        let opcode = step.opcode.unwrap();
        let is_create2 = opcode == OpcodeId::CREATE2;
        self.opcode
            .assign(region, offset, Value::known(F::from(opcode.as_u64())))?;
        self.is_create2.assign(
            region,
            offset,
            Value::known(is_create2.to_scalar().unwrap()),
        )?;

        let [value, initialization_code_start, initialization_code_length] = [0, 1, 2]
            .map(|i| step.rw_indices[i])
            .map(|idx| block.rws[idx].stack_value());
        let salt = if is_create2 {
            block.rws[step.rw_indices[3]].stack_value()
        } else {
            U256::zero()
        };

        let copy_rw_increase = initialization_code_length.as_usize();
        // dopy lookup here advances rw_counter by initialization_code_length;

        let values: Vec<_> = (4 + usize::from(is_create2)
            ..4 + usize::from(is_create2) + initialization_code_length.as_usize())
            .map(|i| block.rws[step.rw_indices[i]].memory_value())
            .collect();
        let mut code_hash = keccak256(&values);
        code_hash.reverse();
        self.code_hash.assign(
            region,
            offset,
            Value::known(RandomLinearCombination::random_linear_combine(
                code_hash,
                block.randomness,
            )),
        )?;

        let tx_access_rw =
            block.rws[step.rw_indices[7 + usize::from(is_create2) + copy_rw_increase]];

        let new_address = tx_access_rw.address().expect("asdfawefasdf");

        for (word, assignment) in [(&self.value, value), (&self.salt, salt)] {
            word.assign(region, offset, Some(assignment.to_le_bytes()))?;
        }
        let initialization_code_address = self.initialization_code.assign(
            region,
            offset,
            initialization_code_start,
            initialization_code_length,
            block.randomness,
        )?;

        let mut bytes = new_address.to_fixed_bytes();
        bytes.reverse();
        self.new_address.assign(region, offset, Some(bytes))?;

        self.tx_id
            .assign(region, offset, Value::known(tx.id.to_scalar().unwrap()))?;

        self.reversion_info.assign(
            region,
            offset,
            call.rw_counter_end_of_reversion,
            call.is_persistent,
        )?;

        self.was_warm.assign(
            region,
            offset,
            Value::known(
                tx_access_rw
                    .tx_access_list_value_pair()
                    .1
                    .to_scalar()
                    .unwrap(),
            ),
        )?;

        let mut bytes = call.callee_address.to_fixed_bytes();
        bytes.reverse();
        self.caller_address.assign(region, offset, Some(bytes))?;

        let nonce_bytes = block.rws
            [step.rw_indices[9 + usize::from(is_create2) + copy_rw_increase]]
            .account_value_pair()
            .1
            .low_u64()
            .to_le_bytes();
        self.nonce.assign(region, offset, Some(nonce_bytes))?;

        let [callee_rw_counter_end_of_reversion, callee_is_persistent] = [10, 11].map(|i| {
            block.rws[step.rw_indices[i + usize::from(is_create2) + copy_rw_increase]]
                .call_context_value()
        });

        self.callee_reversion_info.assign(
            region,
            offset,
            callee_rw_counter_end_of_reversion
                .low_u64()
                .try_into()
                .unwrap(),
            callee_is_persistent.low_u64() != 0,
        )?;

        let [caller_balance_pair, callee_balance_pair] = [13, 14].map(|i| {
            block.rws[step.rw_indices[i + usize::from(is_create2) + copy_rw_increase]]
                .account_value_pair()
        });
        self.transfer.assign(
            region,
            offset,
            caller_balance_pair,
            callee_balance_pair,
            value,
        )?;

        self.memory_expansion.assign(
            region,
            offset,
            step.memory_word_size(),
            [initialization_code_address],
        )?;

        self.gas_left.assign(
            region,
            offset,
            (step.gas_left - GasCost::CREATE.as_u64()).into(),
        )?;

        self.callee_is_success.assign(
            region,
            offset,
            Value::known(
                block.rws[step.rw_indices[21 + usize::from(is_create2) + copy_rw_increase]]
                    .call_context_value()
                    .to_scalar()
                    .unwrap(),
            ),
        )?;

        Ok(())
    }
}
//
// struct Eip150GasGadget<F> {
//     divide_by_64: ConstantDivisionGadget<F, N_BYTES_GAS>,
// }
//
// impl<F:Field> Eip150GasGadget<F> {
//     fn construct() {
//         let gas_cost = GasCost::CREATE.expr() + memory_expansion.gas_cost();
//         let gas_left =
//             ConstantDivisionGadget::construct(cb,
// cb.curr.state.gas_left.expr() - gas_cost, 64);
//
//     }
//
//     fn callee_gas_left(&self) -> Expression<F> {
//
//     }
//
//     fn caller_gas_left(&self) -> Expression<F> {
//
//     }

#[cfg(test)]
mod test {
    use crate::test_util::run_test_circuits;
    use eth_types::{
        address, bytecode, evm_types::OpcodeId, geth_types::Account, Address, Bytecode, ToWord,
        Word,
    };
    use itertools::Itertools;
    use mock::{eth, TestContext};

    const CALLEE_ADDRESS: Address = Address::repeat_byte(0xff);
    const CALLER_ADDRESS: Address = Address::repeat_byte(0x34);

    fn callee_bytecode(is_return: bool, offset: u64, length: u64) -> Bytecode {
        let memory_bytes = [0x60; 10];
        let memory_address = 0;
        let memory_value = Word::from_big_endian(&memory_bytes);
        let mut code = bytecode! {
            PUSH10(memory_value)
            PUSH1(memory_address)
            MSTORE
            PUSH2(length)
            PUSH2(32u64 - u64::try_from(memory_bytes.len()).unwrap() + offset)
        };
        code.write_op(if is_return {
            OpcodeId::RETURN
        } else {
            OpcodeId::REVERT
        });
        code
    }

    fn caller_bytecode(return_data_offset: u64, return_data_length: u64) -> Bytecode {
        bytecode! {
            PUSH32(return_data_length)
            PUSH32(return_data_offset)
            PUSH32(0) // call data length
            PUSH32(0) // call data offset
            PUSH32(0) // value
            PUSH32(CALLEE_ADDRESS.to_word())
            PUSH32(4000) // gas
            CALL
            STOP
        }
    }

    #[test]
    fn mason2() {
        let test_parameters = [(0, 0), (0, 10), (300, 20), (1000, 0)];
        for ((offset, length), is_return) in test_parameters.iter().cartesian_product(&[true])
        // FIX MEEEEEE there's an issue when the init call reverts.
        {
            let initializer = callee_bytecode(*is_return, *offset, *length).code();

            let root_code = bytecode! {
                PUSH32(Word::from_big_endian(&initializer))
                PUSH1(0)
                MSTORE

                PUSH1(initializer.len())        // size
                PUSH1(32 - initializer.len())   // offset
                PUSH1(0)                        // value

                CREATE
            };

            let caller = Account {
                address: CALLER_ADDRESS,
                code: root_code.into(),
                nonce: Word::one(),
                balance: eth(10),
                ..Default::default()
            };

            let test_context = TestContext::<2, 1>::new(
                None,
                |accs| {
                    accs[0]
                        .address(address!("0x000000000000000000000000000000000000cafe"))
                        .balance(eth(10));
                    accs[1].account(&caller);
                },
                |mut txs, accs| {
                    txs[0]
                        .from(accs[0].address)
                        .to(accs[1].address)
                        .gas(100000u64.into());
                },
                |block, _| block,
            )
            .unwrap();

            assert_eq!(
                run_test_circuits(test_context, None),
                Ok(()),
                "(offset, length, is_return) = {:?}",
                (*offset, *length, *is_return),
            );
        }
    }

    #[test]
    fn mason() {
        // Test the case where the initialization call is successful, but the CREATE
        // call is reverted.
        let initializer = callee_bytecode(true, 0, 10).code();

        let root_code = bytecode! {
            PUSH32(Word::from_big_endian(&initializer))
            PUSH1(0)
            MSTORE

            PUSH1(initializer.len())        // size
            PUSH1(32 - initializer.len())   // offset
            PUSH1(0)                        // value

            CREATE
            PUSH1(0)
            PUSH1(0)
            REVERT
        };

        let caller = Account {
            address: CALLER_ADDRESS,
            code: root_code.into(),
            nonce: Word::one(),
            balance: eth(10),
            ..Default::default()
        };

        let test_context = TestContext::<2, 1>::new(
            None,
            |accs| {
                accs[0]
                    .address(address!("0x000000000000000000000000000000000000cafe"))
                    .balance(eth(10));
                accs[1].account(&caller);
            },
            |mut txs, accs| {
                txs[0]
                    .from(accs[0].address)
                    .to(accs[1].address)
                    .gas(100000u64.into());
            },
            |block, _| block,
        )
        .unwrap();

        assert_eq!(run_test_circuits(test_context, None), Ok(()),);
    }
}