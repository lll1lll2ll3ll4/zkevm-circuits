use eth_types::{evm_types::GasCost, Field, ToScalar};
use gadgets::util::Expr;
use halo2_proofs::{circuit::Value, plonk::Error};

use crate::{
    evm_circuit::{
        execution::ExecutionGadget,
        param::{N_BYTES_MEMORY_WORD_SIZE, N_BYTES_WORD},
        step::ExecutionState,
        util::{
            common_gadget::RestoreContextGadget,
            constraint_builder::{ConstrainBuilderCommon, EVMConstraintBuilder},
            math_gadget::ConstantDivisionGadget,
            CachedRegion, Cell,
        },
    },
    table::CallContextFieldTag,
    witness::{Block, Call, ExecStep, Transaction},
};

#[derive(Clone, Debug)]
pub struct IdentityGadget<F> {
    gas_cost: Cell<F>,
    input_word_size: ConstantDivisionGadget<F, N_BYTES_MEMORY_WORD_SIZE>,

    is_success: Cell<F>,
    callee_address: Cell<F>,
    caller_id: Cell<F>,
    call_data_offset: Cell<F>,
    call_data_length: Cell<F>,
    return_data_offset: Cell<F>,
    return_data_length: Cell<F>,
    restore_context: RestoreContextGadget<F>,
}

impl<F: Field> ExecutionGadget<F> for IdentityGadget<F> {
    const EXECUTION_STATE: ExecutionState = ExecutionState::PrecompileIdentity;

    const NAME: &'static str = "IDENTITY";

    fn configure(cb: &mut EVMConstraintBuilder<F>) -> Self {
        let gas_cost = cb.query_cell();

        let [is_success, callee_address, caller_id, call_data_offset, call_data_length, return_data_offset, return_data_length] =
            [
                CallContextFieldTag::IsSuccess,
                CallContextFieldTag::CalleeAddress,
                CallContextFieldTag::CallerId,
                CallContextFieldTag::CallDataOffset,
                CallContextFieldTag::CallDataLength,
                CallContextFieldTag::ReturnDataOffset,
                CallContextFieldTag::ReturnDataLength,
            ]
            .map(|tag| cb.call_context(None, tag));

        let input_word_size = ConstantDivisionGadget::construct(
            cb,
            call_data_length.expr() + (N_BYTES_WORD - 1).expr(),
            N_BYTES_WORD as u64,
        );
        cb.require_equal(
            "ecrcover: gas cost",
            gas_cost.expr(),
            GasCost::PRECOMPILE_IDENTITY_BASE.expr()
                + input_word_size.quotient() * GasCost::PRECOMPILE_IDENTITY_PER_WORD.expr(),
        );

        cb.precompile_info_lookup(
            cb.execution_state().as_u64().expr(),
            callee_address.expr(),
            cb.execution_state().precompile_base_gas_cost().expr(),
        );

        let restore_context = RestoreContextGadget::construct2(
            cb,
            is_success.expr(),
            gas_cost.expr(),
            0.expr(),
            0x00.expr(),             // ReturnDataOffset
            call_data_length.expr(), // ReturnDataLength
            0.expr(),
            0.expr(),
        );

        Self {
            gas_cost,
            input_word_size,

            is_success,
            callee_address,
            caller_id,
            call_data_offset,
            call_data_length,
            return_data_offset,
            return_data_length,
            restore_context,
        }
    }

    fn assign_exec_step(
        &self,
        region: &mut CachedRegion<'_, '_, F>,
        offset: usize,
        block: &Block<F>,
        _tx: &Transaction,
        call: &Call,
        step: &ExecStep,
    ) -> Result<(), Error> {
        let gas_cost = GasCost::PRECOMPILE_IDENTITY_BASE.0
            + ((call.call_data_length + (N_BYTES_WORD as u64) - 1) / (N_BYTES_WORD as u64))
                * GasCost::PRECOMPILE_IDENTITY_PER_WORD.0;
        debug_assert_eq!(gas_cost, step.gas_cost);
        self.gas_cost
            .assign(region, offset, Value::known(F::from(gas_cost)))?;
        self.input_word_size.assign(
            region,
            offset,
            (call.call_data_length + (N_BYTES_WORD as u64) - 1).into(),
        )?;

        self.is_success.assign(
            region,
            offset,
            Value::known(F::from(u64::from(call.is_success))),
        )?;
        self.callee_address.assign(
            region,
            offset,
            Value::known(call.code_address.unwrap().to_scalar().unwrap()),
        )?;
        self.caller_id
            .assign(region, offset, Value::known(F::from(call.caller_id as u64)))?;
        self.call_data_offset.assign(
            region,
            offset,
            Value::known(F::from(call.call_data_offset)),
        )?;
        self.call_data_length.assign(
            region,
            offset,
            Value::known(F::from(call.call_data_length)),
        )?;
        self.return_data_offset.assign(
            region,
            offset,
            Value::known(F::from(call.return_data_offset)),
        )?;
        self.return_data_length.assign(
            region,
            offset,
            Value::known(F::from(call.return_data_length)),
        )?;

        self.restore_context
            .assign(region, offset, block, call, step, 7)
    }
}

#[cfg(test)]
mod test {
    use bus_mapping::{
        evm::{OpcodeId, PrecompileCallArgs},
        precompile::PrecompileCalls,
    };
    use eth_types::{bytecode, word, ToWord};
    use itertools::Itertools;
    use mock::TestContext;

    use crate::test_util::CircuitTestBuilder;

    lazy_static::lazy_static! {
        static ref TEST_VECTOR: Vec<PrecompileCallArgs> = {
            vec![
                PrecompileCallArgs {
                    name: "single-byte success",
                    setup_code: bytecode! {
                        // place params in memory
                        PUSH1(0xff)
                        PUSH1(0x00)
                        MSTORE
                    },
                    call_data_offset: 0x1f.into(),
                    call_data_length: 0x01.into(),
                    ret_offset: 0x3f.into(),
                    ret_size: 0x01.into(),
                    address: PrecompileCalls::Identity.address().to_word(),
                    ..Default::default()
                },
                PrecompileCallArgs {
                    name: "multi-bytes success (less than 32 bytes)",
                    setup_code: bytecode! {
                        // place params in memory
                        PUSH16(word!("0x0123456789abcdef0f1e2d3c4b5a6978"))
                        PUSH1(0x00)
                        MSTORE
                    },
                    call_data_offset: 0x00.into(),
                    call_data_length: 0x10.into(),
                    ret_offset: 0x20.into(),
                    ret_size: 0x10.into(),
                    address: PrecompileCalls::Identity.address().to_word(),
                    ..Default::default()
                },
                PrecompileCallArgs {
                    name: "multi-bytes success (more than 32 bytes)",
                    setup_code: bytecode! {
                        // place params in memory
                        PUSH30(word!("0x0123456789abcdef0f1e2d3c4b5a6978"))
                        PUSH1(0x00) // place from 0x00 in memory
                        MSTORE
                        PUSH30(word!("0xaabbccdd001122331039abcdefefef84"))
                        PUSH1(0x20) // place from 0x20 in memory
                        MSTORE
                    },
                    // copy 63 bytes from memory addr 0
                    call_data_offset: 0x00.into(),
                    call_data_length: 0x3f.into(),
                    // return only 35 bytes and write from memory addr 72
                    ret_offset: 0x48.into(),
                    ret_size: 0x23.into(),
                    address: PrecompileCalls::Identity.address().to_word(),
                    ..Default::default()
                },
            ]
        };
    }

    #[test]
    fn precompile_identity_test() {
        let call_kinds = vec![
            OpcodeId::CALL,
            OpcodeId::STATICCALL,
            OpcodeId::DELEGATECALL,
            OpcodeId::CALLCODE,
        ];

        for (test_vector, &call_kind) in TEST_VECTOR.iter().cartesian_product(&call_kinds) {
            let bytecode = test_vector.with_call_op(call_kind);

            CircuitTestBuilder::new_from_test_ctx(
                TestContext::<2, 1>::simple_ctx_with_bytecode(bytecode).unwrap(),
            )
            .run();
        }
    }
}
