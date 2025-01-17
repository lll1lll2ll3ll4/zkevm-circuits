//! The ECC circuit is responsible for verifying ECC-related operations from precompiled contract
//! calls, namely, EcAdd, EcMul and EcPairing.

use std::{iter, marker::PhantomData};

use bus_mapping::{
    circuit_input_builder::{EcAddOp, EcMulOp, EcPairingOp, N_BYTES_PER_PAIR, N_PAIRING_PER_OP},
    precompile::PrecompileCalls,
};
use eth_types::{Field, ToScalar};
use halo2_base::{
    gates::{GateInstructions, RangeInstructions},
    utils::modulus,
    Context, QuantumCell, SKIP_FIRST_PASS,
};
use halo2_ecc::{
    bigint::CRTInteger,
    bn254::pairing::PairingChip,
    ecc::EccChip,
    fields::{
        fp::{FpConfig, FpStrategy},
        fp12::Fp12Chip,
        FieldChip,
    },
};
use halo2_proofs::{
    arithmetic::Field as Halo2Field,
    circuit::{Layouter, Value},
    halo2curves::bn256::{Fq, Fq12, Fr, G1Affine, G2Affine},
    plonk::{ConstraintSystem, Error, Expression},
};
use itertools::Itertools;
use log::error;

use crate::{
    evm_circuit::{param::N_BYTES_WORD, EvmCircuit},
    keccak_circuit::KeccakCircuit,
    table::{EccTable, LookupTable},
    util::{Challenges, SubCircuit, SubCircuitConfig},
    witness::Block,
};

mod dev;
mod test;
mod util;

use util::{
    EcAddAssigned, EcAddDecomposed, EcMulAssigned, EcMulDecomposed, EcOpsAssigned,
    EcPairingAssigned, EcPairingDecomposed, G1Assigned, G1Decomposed, G2Decomposed, ScalarAssigned,
    LOG_TOTAL_NUM_ROWS,
};

macro_rules! log_context_cursor {
    ($ctx: ident) => {{
        log::trace!("Ctx cell pos: {:?}", $ctx.advice_alloc);
    }};
}

/// Arguments accepted to configure the EccCircuitConfig.
#[derive(Clone, Debug)]
pub struct EccCircuitConfigArgs<F: Field> {
    /// ECC table that is connected to the ECC circuit.
    pub ecc_table: EccTable,
    /// zkEVM challenge API.
    pub challenges: Challenges<Expression<F>>,
}

/// Config for the ECC circuit.
#[derive(Clone, Debug)]
pub struct EccCircuitConfig<F: Field> {
    /// Field config for halo2_proofs::halo2curves::bn256::Fq.
    fp_config: FpConfig<F, Fq>,
    /// Lookup table for I/Os to the EcAdd, EcMul and EcPairing operations.
    ecc_table: EccTable,

    /// Number of limbs to represent Fp.
    num_limbs: usize,
    /// Number of bits per limb.
    limb_bits: usize,

    _marker: PhantomData<F>,
}

impl<F: Field> SubCircuitConfig<F> for EccCircuitConfig<F> {
    type ConfigArgs = EccCircuitConfigArgs<F>;

    fn new(
        meta: &mut ConstraintSystem<F>,
        Self::ConfigArgs {
            ecc_table,
            challenges: _,
        }: Self::ConfigArgs,
    ) -> Self {
        let num_limbs = 3;
        let limb_bits = 88;
        #[cfg(feature = "onephase")]
        let num_advice = [33];
        #[cfg(not(feature = "onephase"))]
        let num_advice = [33, 1];

        let fp_config = FpConfig::configure(
            meta,
            FpStrategy::Simple,
            &num_advice,
            &[17], // num lookup advice
            1,     // num fixed
            13,    // lookup bits
            limb_bits,
            num_limbs,
            modulus::<Fq>(),
            0,
            LOG_TOTAL_NUM_ROWS as usize, // k
        );

        for column in <EccTable as LookupTable<F>>::advice_columns(&ecc_table) {
            meta.enable_equality(column);
        }

        Self {
            fp_config,
            ecc_table,
            num_limbs,
            limb_bits,
            _marker: PhantomData,
        }
    }
}

/// The ECC Circuit is a sub-circuit of the super circuit, responsible for verifying the following
/// ECC operations:
/// 1. Point Addition (R = P + Q)
/// 2. Scalar Multiplication (R = s.P)
/// 3. Pairing-based bilinear function
///
/// We follow a strategy to pre-allocate maximum number of cells for each of the above ECC
/// operations, which means a witness that exceeds the pre-allocated number of cells for any of the
/// operations will be invalid.
#[derive(Clone, Debug, Default)]
pub struct EccCircuit<F: Field, const XI_0: i64> {
    /// Maximum number of EcAdd operations supported in one instance of the ECC Circuit.
    pub max_add_ops: usize,
    /// Maximum number of scalar multiplication operations supported in one instance of the ECC
    /// Circuit.
    pub max_mul_ops: usize,
    /// Maximum number of pairing operations supported in one instance of the ECC Circuit.
    pub max_pairing_ops: usize,

    /// EcAdd operations provided as witness data to the ECC circuit.
    pub add_ops: Vec<EcAddOp>,
    /// EcMul operations provided as witness data to the ECC circuit.
    pub mul_ops: Vec<EcMulOp>,
    /// EcPairing operations provided as witness data to the ECC circuit.
    pub pairing_ops: Vec<EcPairingOp>,

    _marker: PhantomData<F>,
}

impl<F: Field, const XI_0: i64> EccCircuit<F, XI_0> {
    /// Assign witness from the ecXX ops to the circuit.
    pub(crate) fn assign(
        &self,
        layouter: &mut impl Layouter<F>,
        config: &<Self as SubCircuit<F>>::Config,
        challenges: &Challenges<Value<F>>,
    ) -> Result<(), Error> {
        if self.add_ops.len() > self.max_add_ops
            || self.mul_ops.len() > self.max_mul_ops
            || self.pairing_ops.len() > self.max_pairing_ops
        {
            error!(
                "add ops = {}, mul ops = {}, pairing ops = {} > max add ops = {}, max mul ops = {}, max pairing ops = {}",
                self.add_ops.len(),
                self.mul_ops.len(),
                self.pairing_ops.len(),
                self.max_add_ops,
                self.max_mul_ops,
                self.max_pairing_ops,
            );
            return Err(Error::Synthesis);
        }

        // keccak powers of randomness.
        let keccak_powers = std::iter::successors(Some(Value::known(F::one())), |coeff| {
            Some(challenges.keccak_input() * coeff)
        })
        .take(N_PAIRING_PER_OP * N_BYTES_PER_PAIR)
        .map(|x| QuantumCell::Witness(x))
        .collect_vec();

        let powers_of_256 = iter::successors(Some(F::one()), |coeff| Some(F::from(256) * coeff))
            .take(N_BYTES_WORD)
            .map(|x| QuantumCell::Constant(x))
            .collect_vec();

        let ecc_chip = EccChip::<F, FpConfig<F, Fq>>::construct(config.fp_config.clone());
        let fr_chip = FpConfig::<F, Fr>::construct(
            config.fp_config.range.clone(),
            config.limb_bits,
            config.num_limbs,
            modulus::<Fr>(),
        );
        let pairing_chip = PairingChip::construct(config.fp_config.clone());
        let fp12_chip =
            Fp12Chip::<F, FpConfig<F, Fq>, Fq12, XI_0>::construct(config.fp_config.clone());

        let mut first_pass = SKIP_FIRST_PASS;

        let assigned_ec_ops = layouter.assign_region(
            || "ecc circuit",
            |region| {
                if first_pass {
                    first_pass = false;
                    return Ok(EcOpsAssigned::default());
                }

                let mut ctx = config.fp_config.new_context(region);

                macro_rules! decompose_ec_op {
                    ($op_type:ident, $ops:expr, $n_ops:expr, $decompose_fn:ident) => {
                        $ops.iter()
                            .filter(|op| !op.skip_by_ecc_circuit())
                            .chain(std::iter::repeat(&$op_type::default()))
                            .take($n_ops)
                            .map(|op| {
                                self.$decompose_fn(
                                    &mut ctx,
                                    &ecc_chip,
                                    &fr_chip,
                                    &pairing_chip,
                                    &fp12_chip,
                                    &powers_of_256,
                                    &op,
                                )
                            })
                            .collect_vec()
                    };
                }

                macro_rules! assign_ec_op {
                    ($decomposed_ops:expr, $assign_fn:ident) => {
                        $decomposed_ops
                            .iter()
                            .map(|decomposed_op| {
                                self.$assign_fn(&mut ctx, decomposed_op, &ecc_chip, &keccak_powers)
                            })
                            .collect_vec()
                    };
                }

                // P + Q == R
                let ec_adds_decomposed =
                    decompose_ec_op!(EcAddOp, self.add_ops, self.max_add_ops, decompose_ec_add_op);

                // s.P = R
                let ec_muls_decomposed =
                    decompose_ec_op!(EcMulOp, self.mul_ops, self.max_mul_ops, decompose_ec_mul_op);

                // e(G1 . G2) * ... * e(G1 . G2) -> Gt
                let ec_pairings_decomposed = decompose_ec_op!(
                    EcPairingOp,
                    self.pairing_ops,
                    self.max_pairing_ops,
                    decompose_ec_pairing_op
                );

                #[cfg(not(feature = "onephase"))]
                {
                    // finalize after first phase.
                    config.fp_config.finalize(&mut ctx);
                    ctx.next_phase();
                }

                let ec_adds_assigned = assign_ec_op!(ec_adds_decomposed, assign_ec_add);
                let ec_muls_assigned = assign_ec_op!(ec_muls_decomposed, assign_ec_mul);
                let ec_pairings_assigned = assign_ec_op!(ec_pairings_decomposed, assign_ec_pairing);

                // Finalize the Fp config always at the end of assignment.
                let lookup_cells = config.fp_config.finalize(&mut ctx);
                log::info!("total number of lookup cells: {}", lookup_cells);
                ctx.print_stats(&["EccCircuit: FpConfig context"]);

                Ok(EcOpsAssigned {
                    ec_adds_assigned,
                    ec_muls_assigned,
                    ec_pairings_assigned,
                })
            },
        )?;

        layouter.assign_region(
            || "expose ecc table",
            |mut region| {
                // handle EcAdd ops.
                for (idx, ec_add_assigned) in assigned_ec_ops.ec_adds_assigned.iter().enumerate() {
                    region.assign_fixed(
                        || "assign ecc_table op_type",
                        config.ecc_table.op_type,
                        idx,
                        || Value::known(F::from(u64::from(PrecompileCalls::Bn128Add))),
                    )?;
                    // P_x
                    ec_add_assigned.point_p.x_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg1_rlc,
                        idx,
                    );
                    // P_y
                    ec_add_assigned.point_p.y_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg2_rlc,
                        idx,
                    );
                    // Q_x
                    ec_add_assigned.point_q.x_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg3_rlc,
                        idx,
                    );
                    // Q_y
                    ec_add_assigned.point_q.y_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg4_rlc,
                        idx,
                    );
                    // R_x
                    ec_add_assigned.point_r.x_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.output1_rlc,
                        idx,
                    );
                    // R_y
                    ec_add_assigned.point_r.y_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.output2_rlc,
                        idx,
                    );
                    // input_rlc == 0
                    region.assign_advice(
                        || format!("input_rlc at offset = {idx}"),
                        config.ecc_table.input_rlc,
                        idx,
                        || Value::known(F::zero()),
                    )?;
                }

                // handle EcMul ops.
                for (idx, ec_mul_assigned) in assigned_ec_ops.ec_muls_assigned.iter().enumerate() {
                    let idx = idx + self.max_add_ops;
                    region.assign_fixed(
                        || "assign ecc_table op_type",
                        config.ecc_table.op_type,
                        idx,
                        || Value::known(F::from(u64::from(PrecompileCalls::Bn128Mul))),
                    )?;
                    // P_x
                    ec_mul_assigned.point_p.x_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg1_rlc,
                        idx,
                    );
                    // P_y
                    ec_mul_assigned.point_p.y_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.arg2_rlc,
                        idx,
                    );
                    // Scalar s
                    ec_mul_assigned.scalar_s.scalar.native.copy_advice(
                        &mut region,
                        config.ecc_table.arg3_rlc,
                        idx,
                    );
                    // R_x
                    ec_mul_assigned.point_r.x_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.output1_rlc,
                        idx,
                    );
                    // R_y
                    ec_mul_assigned.point_r.y_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.output2_rlc,
                        idx,
                    );
                    for &col in [config.ecc_table.arg4_rlc, config.ecc_table.input_rlc].iter() {
                        region.assign_advice(
                            || format!("{col:?} at offset = {idx}"),
                            col,
                            idx,
                            || Value::known(F::zero()),
                        )?;
                    }
                }

                // handle EcPairing ops.
                for (idx, ec_pairing_assigned) in
                    assigned_ec_ops.ec_pairings_assigned.iter().enumerate()
                {
                    let idx = idx + self.max_add_ops + self.max_mul_ops;
                    region.assign_fixed(
                        || "assign ecc_table op_type",
                        config.ecc_table.op_type,
                        idx,
                        || Value::known(F::from(u64::from(PrecompileCalls::Bn128Pairing))),
                    )?;
                    // RLC(input_bytes)
                    ec_pairing_assigned.input_rlc.copy_advice(
                        &mut region,
                        config.ecc_table.input_rlc,
                        idx,
                    );
                    // success
                    ec_pairing_assigned.success.copy_advice(
                        &mut region,
                        config.ecc_table.output1_rlc,
                        idx,
                    );
                    for &col in [
                        config.ecc_table.arg1_rlc,
                        config.ecc_table.arg2_rlc,
                        config.ecc_table.arg3_rlc,
                        config.ecc_table.arg4_rlc,
                        config.ecc_table.output2_rlc,
                    ]
                    .iter()
                    {
                        region.assign_advice(
                            || format!("{col:?} at offset = {idx}"),
                            col,
                            idx,
                            || Value::known(F::zero()),
                        )?;
                    }
                }

                Ok(())
            },
        )?;

        Ok(())
    }

    /// Decomposes an EcAdd operation to return each G1 element as cells representing its byte
    /// form.
    #[allow(clippy::too_many_arguments)]
    fn decompose_ec_add_op(
        &self,
        ctx: &mut Context<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        _fr_chip: &FpConfig<F, Fr>,
        _pairing_chip: &PairingChip<F>,
        _fp12_chip: &Fp12Chip<F, FpConfig<F, Fq>, Fq12, XI_0>,
        powers_of_256: &[QuantumCell<F>],
        op: &EcAddOp,
    ) -> EcAddDecomposed<F> {
        log::trace!("[ECC] ==> EcAdd Assignmnet START:");
        log_context_cursor!(ctx);

        let point_p = self.assign_g1(ctx, ecc_chip, op.p, powers_of_256);
        let point_q = self.assign_g1(ctx, ecc_chip, op.q, powers_of_256);
        let point_r = self.assign_g1(ctx, ecc_chip, op.r, powers_of_256);

        log::trace!("[ECC] EcAdd Inputs Assigned:");
        log_context_cursor!(ctx);

        // We follow the approach mentioned below to handle many edge cases for the points P, Q and
        // R so that we can maintain the same fixed and permutation columns and reduce the overall
        // validation process from the EVM Circuit.
        //
        // To check the validity of P + Q == R, we check:
        // r + P + Q - R == r
        // where r is a random point on the curve.
        //
        // We cover cases such as:
        // - P == (0, 0) and/or Q == (0, 0)
        // - P == -Q, i.e. P + Q == R == (0, 0)

        let rand_point = ecc_chip.load_random_point::<G1Affine>(ctx);

        // check if P == (0, 0), Q == (0, 0), R == (0, 0)
        let p_x_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_p.ec_point.x);
        let p_y_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_p.ec_point.y);
        let q_x_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_q.ec_point.x);
        let q_y_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_q.ec_point.y);
        let r_x_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_r.ec_point.x);
        let r_y_is_zero = ecc_chip.field_chip.is_zero(ctx, &point_r.ec_point.y);
        let point_p_is_zero = ecc_chip.field_chip.range().gate().and(
            ctx,
            QuantumCell::Existing(p_x_is_zero),
            QuantumCell::Existing(p_y_is_zero),
        );
        let point_q_is_zero = ecc_chip.field_chip.range().gate().and(
            ctx,
            QuantumCell::Existing(q_x_is_zero),
            QuantumCell::Existing(q_y_is_zero),
        );
        let point_r_is_zero = ecc_chip.field_chip.range().gate().and(
            ctx,
            QuantumCell::Existing(r_x_is_zero),
            QuantumCell::Existing(r_y_is_zero),
        );

        // sum1 = if P == (0, 0) then r else r + P
        let sum1 = ecc_chip.add_unequal(ctx, &rand_point, &point_p.ec_point, true);
        let sum1 = ecc_chip.select(ctx, &rand_point, &sum1, &point_p_is_zero);

        // sum2 = if Q == (0, 0) then sum1 else sum1 + Q
        let sum2 = ecc_chip.add_unequal(ctx, &sum1, &point_q.ec_point, true);
        let sum2 = ecc_chip.select(ctx, &sum1, &sum2, &point_q_is_zero);

        // sum3 = if R == (0, 0) then sum2 else sum2 - R
        let sum3 = ecc_chip.sub_unequal(ctx, &sum2, &point_r.ec_point, true);
        let sum3 = ecc_chip.select(ctx, &sum2, &sum3, &point_r_is_zero);

        ecc_chip.assert_equal(ctx, &rand_point, &sum3);

        log::trace!("[ECC] EcAdd Assignmnet END:");
        log_context_cursor!(ctx);
        EcAddDecomposed {
            point_p,
            point_q,
            point_r,
        }
    }

    /// Decomposes an EcMul operation to return each G1 element as cells representing its byte
    /// form.
    #[allow(clippy::too_many_arguments)]
    fn decompose_ec_mul_op(
        &self,
        ctx: &mut Context<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        fr_chip: &FpConfig<F, Fr>,
        _pairing_chip: &PairingChip<F>,
        _fp12_chip: &Fp12Chip<F, FpConfig<F, Fq>, Fq12, XI_0>,
        powers_of_256: &[QuantumCell<F>],
        op: &EcMulOp,
    ) -> EcMulDecomposed<F> {
        log::trace!("[ECC] ==> EcMul Assignmnet START:");
        log_context_cursor!(ctx);

        let point_p = self.assign_g1(ctx, ecc_chip, op.p, powers_of_256);
        let scalar_s = self.assign_fr(ctx, fr_chip, op.s);
        let point_r = self.assign_g1(ctx, ecc_chip, op.r, powers_of_256);

        log::trace!("[ECC] EcMul Inputs Assigned:");
        log_context_cursor!(ctx);

        let point_r_got = ecc_chip.scalar_mult(
            ctx,
            &point_p.ec_point,
            &scalar_s.scalar.limbs().to_vec(),
            fr_chip.limb_bits,
            4, // TODO: window bits?
        );
        ecc_chip.assert_equal(ctx, &point_r.ec_point, &point_r_got);

        log::trace!("[ECC] EcMul Assignmnet END:");
        log_context_cursor!(ctx);

        EcMulDecomposed {
            point_p,
            scalar_s,
            point_r,
        }
    }

    /// Decomposes an EcPairing operation and returns cells that represent the LE-bytes of all
    /// (G1, G2) pairs. In phase2 they will be RLC'd with the keccak randomness.
    #[allow(clippy::too_many_arguments)]
    fn decompose_ec_pairing_op(
        &self,
        ctx: &mut Context<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        _fr_chip: &FpConfig<F, Fr>,
        pairing_chip: &PairingChip<F>,
        fp12_chip: &Fp12Chip<F, FpConfig<F, Fq>, Fq12, XI_0>,
        powers_of_256: &[QuantumCell<F>],
        op: &EcPairingOp,
    ) -> EcPairingDecomposed<F> {
        log::trace!("[ECC] ==> EcPairing Assignment START:");
        log_context_cursor!(ctx);

        let g1s = op
            .pairs
            .iter()
            .map(|pair| {
                let ec_point = pairing_chip.load_private_g1(ctx, Value::known(pair.g1_point));
                let (x_cells, y_cells) = self.decompose_g1(pair.g1_point);
                self.assert_crt_repr(ctx, ecc_chip, &ec_point.x, &x_cells, powers_of_256);
                self.assert_crt_repr(ctx, ecc_chip, &ec_point.y, &y_cells, powers_of_256);
                G1Decomposed {
                    ec_point,
                    x_cells,
                    y_cells,
                }
            })
            .collect_vec();

        log::trace!("[ECC] EcPairing g1s Assigned:");
        log_context_cursor!(ctx);

        let g2s = op
            .pairs
            .iter()
            .map(|pair| {
                let ec_point = pairing_chip.load_private_g2(ctx, Value::known(pair.g2_point));
                let [x_c0_cells, x_c1_cells, y_c0_cells, y_c1_cells] =
                    self.decompose_g2(pair.g2_point);
                self.assert_crt_repr(
                    ctx,
                    ecc_chip,
                    &ec_point.x.coeffs[0],
                    &x_c0_cells,
                    powers_of_256,
                );
                self.assert_crt_repr(
                    ctx,
                    ecc_chip,
                    &ec_point.x.coeffs[1],
                    &x_c1_cells,
                    powers_of_256,
                );
                self.assert_crt_repr(
                    ctx,
                    ecc_chip,
                    &ec_point.y.coeffs[0],
                    &y_c0_cells,
                    powers_of_256,
                );
                self.assert_crt_repr(
                    ctx,
                    ecc_chip,
                    &ec_point.y.coeffs[1],
                    &y_c1_cells,
                    powers_of_256,
                );
                G2Decomposed {
                    ec_point,
                    x_c0_cells,
                    x_c1_cells,
                    y_c0_cells,
                    y_c1_cells,
                }
            })
            .collect_vec();

        log::trace!("[ECC] EcPairing g2s Assigned:");
        log_context_cursor!(ctx);

        // RLC over the entire input bytes.
        let input_cells = g1s
            .iter()
            .zip_eq(g2s.iter())
            .flat_map(|(g1, g2)| {
                std::iter::empty()
                    .chain(g1.x_cells.iter().rev())
                    .chain(g1.y_cells.iter().rev())
                    .chain(g2.x_c1_cells.iter().rev())
                    .chain(g2.x_c0_cells.iter().rev())
                    .chain(g2.y_c1_cells.iter().rev())
                    .chain(g2.y_c0_cells.iter().rev())
                    .cloned()
                    .collect::<Vec<QuantumCell<F>>>()
            })
            .collect::<Vec<QuantumCell<F>>>();

        log::trace!("[ECC] EcPairing Inputs RLC Assigned:");
        log_context_cursor!(ctx);

        let pairs = g1s
            .iter()
            .zip(g2s.iter())
            .map(|(g1, g2)| (&g1.ec_point, &g2.ec_point))
            .collect_vec();

        let success = {
            let gt = {
                let gt = pairing_chip.multi_miller_loop(ctx, pairs);
                pairing_chip.final_exp(ctx, &gt)
            };
            // whether pairing check was successful.
            let one = fp12_chip.load_constant(ctx, Fq12::one());
            fp12_chip.is_equal(ctx, &gt, &one)
        };

        let op_output = ecc_chip.field_chip().range().gate().load_witness(
            ctx,
            Value::known(op.output.to_scalar().expect("EcPairing output = {0, 1}")),
        );
        ecc_chip.field_chip().range().gate().assert_equal(
            ctx,
            QuantumCell::Existing(success),
            QuantumCell::Existing(op_output),
        );

        log::trace!("[ECC] EcPairingAssignment END:");
        log_context_cursor!(ctx);

        EcPairingDecomposed {
            input_cells,
            success,
        }
    }

    /// Handles Phase2 for EcAdd operation and returns the RLC'd x and y co-ordinates of the G1
    /// elements.
    fn assign_ec_add(
        &self,
        ctx: &mut Context<F>,
        ec_add_decomposed: &EcAddDecomposed<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        keccak_powers: &[QuantumCell<F>],
    ) -> EcAddAssigned<F> {
        EcAddAssigned {
            point_p: G1Assigned {
                x_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_p.x_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
                y_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_p.y_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
            },
            point_q: G1Assigned {
                x_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_q.x_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
                y_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_q.y_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
            },
            point_r: G1Assigned {
                x_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_r.x_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
                y_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_add_decomposed.point_r.y_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
            },
        }
    }

    /// Handles Phase2 for EcMul operation and returns the RLC'd x and y co-ordinates of the G1
    /// elements, and the assigned scalar field element.
    fn assign_ec_mul(
        &self,
        ctx: &mut Context<F>,
        ec_mul_decomposed: &EcMulDecomposed<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        keccak_powers: &[QuantumCell<F>],
    ) -> EcMulAssigned<F> {
        EcMulAssigned {
            point_p: G1Assigned {
                x_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_mul_decomposed.point_p.x_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
                y_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_mul_decomposed.point_p.y_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
            },
            scalar_s: ec_mul_decomposed.scalar_s.clone(),
            point_r: G1Assigned {
                x_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_mul_decomposed.point_r.x_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
                y_rlc: ecc_chip.field_chip().range().gate().inner_product(
                    ctx,
                    ec_mul_decomposed.point_r.y_cells.clone(),
                    keccak_powers.iter().cloned(),
                ),
            },
        }
    }

    /// Handles Phase2 for EcPairing operation and returns the RLC'd input bytes.
    fn assign_ec_pairing(
        &self,
        ctx: &mut Context<F>,
        ec_pairing_decomposed: &EcPairingDecomposed<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        keccak_powers: &[QuantumCell<F>],
    ) -> EcPairingAssigned<F> {
        EcPairingAssigned {
            input_rlc: ecc_chip.field_chip().range().gate().inner_product(
                ctx,
                ec_pairing_decomposed.input_cells.clone().into_iter().rev(),
                keccak_powers.iter().cloned(),
            ),
            success: ec_pairing_decomposed.success,
        }
    }

    /// Assign the G1 EcPoint and return its decomposed state.
    fn assign_g1(
        &self,
        ctx: &mut Context<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        g1: G1Affine,
        powers_of_256: &[QuantumCell<F>],
    ) -> G1Decomposed<F> {
        let ec_point = ecc_chip.load_private(ctx, (Value::known(g1.x), Value::known(g1.y)));
        let (x_cells, y_cells) = self.decompose_g1(g1);
        self.assert_crt_repr(ctx, ecc_chip, &ec_point.x, &x_cells, powers_of_256);
        self.assert_crt_repr(ctx, ecc_chip, &ec_point.y, &y_cells, powers_of_256);
        G1Decomposed {
            ec_point,
            x_cells,
            y_cells,
        }
    }

    /// Assert that a CRT integer's bytes representation is correct.
    fn assert_crt_repr(
        &self,
        ctx: &mut Context<F>,
        ecc_chip: &EccChip<F, FpConfig<F, Fq>>,
        crt_int: &CRTInteger<F>,
        bytes: &[QuantumCell<F>],
        powers_of_256: &[QuantumCell<F>],
    ) {
        debug_assert_eq!(bytes.len(), 32);
        debug_assert!(powers_of_256.len() >= 11);

        let limbs = [
            bytes[0..11].to_vec(),
            bytes[11..22].to_vec(),
            bytes[22..32].to_vec(),
        ]
        .map(|limb_bytes| {
            ecc_chip.field_chip().range().gate().inner_product(
                ctx,
                limb_bytes,
                powers_of_256[0..11].to_vec(),
            )
        });

        for (&limb_recovered, &limb_value) in limbs.iter().zip_eq(crt_int.truncation.limbs.iter()) {
            ecc_chip.field_chip().range().gate().assert_equal(
                ctx,
                QuantumCell::Existing(limb_recovered),
                QuantumCell::Existing(limb_value),
            );
        }
    }

    /// Decompose G1 element into cells representing its x and y co-ordinates.
    fn decompose_g1(&self, g1: G1Affine) -> (Vec<QuantumCell<F>>, Vec<QuantumCell<F>>) {
        (
            g1.x.to_bytes()
                .iter()
                .map(|&x| QuantumCell::Witness(Value::known(F::from(u64::from(x)))))
                .collect_vec(),
            g1.y.to_bytes()
                .iter()
                .map(|&y| QuantumCell::Witness(Value::known(F::from(u64::from(y)))))
                .collect_vec(),
        )
    }

    /// Decompose G2 element into cells representing all coefficients of its x and y co-ordinates.
    fn decompose_g2(&self, g2: G2Affine) -> [Vec<QuantumCell<F>>; 4] {
        [
            g2.x.c0
                .to_bytes()
                .iter()
                .map(|&x| QuantumCell::Witness(Value::known(F::from(u64::from(x)))))
                .collect_vec(),
            g2.x.c1
                .to_bytes()
                .iter()
                .map(|&x| QuantumCell::Witness(Value::known(F::from(u64::from(x)))))
                .collect_vec(),
            g2.y.c0
                .to_bytes()
                .iter()
                .map(|&y| QuantumCell::Witness(Value::known(F::from(u64::from(y)))))
                .collect_vec(),
            g2.y.c1
                .to_bytes()
                .iter()
                .map(|&y| QuantumCell::Witness(Value::known(F::from(u64::from(y)))))
                .collect_vec(),
        ]
    }

    /// Assign a scalar field element and return its assigned state.
    fn assign_fr(
        &self,
        ctx: &mut Context<F>,
        fr_chip: &FpConfig<F, Fr>,
        s: Fr,
    ) -> ScalarAssigned<F> {
        let scalar = fr_chip.load_private(ctx, FpConfig::<F, Fr>::fe_to_witness(&Value::known(s)));
        ScalarAssigned { scalar }
    }
}

impl<F: Field, const XI_0: i64> SubCircuit<F> for EccCircuit<F, XI_0> {
    type Config = EccCircuitConfig<F>;

    fn new_from_block(block: &Block<F>) -> Self {
        Self {
            max_add_ops: block.circuits_params.max_ec_ops.ec_add,
            max_mul_ops: block.circuits_params.max_ec_ops.ec_mul,
            max_pairing_ops: block.circuits_params.max_ec_ops.ec_pairing,
            add_ops: block.get_ec_add_ops(),
            mul_ops: block.get_ec_mul_ops(),
            pairing_ops: block.get_ec_pairing_ops(),
            _marker: PhantomData,
        }
    }

    /// Returns number of unusable rows of the SubCircuit, which should be
    /// `meta.blinding_factors() + 1`.
    fn unusable_rows() -> usize {
        [
            KeccakCircuit::<F>::unusable_rows(),
            EvmCircuit::<F>::unusable_rows(),
            // may include additional subcircuits here
        ]
        .into_iter()
        .max()
        .unwrap()
    }

    fn synthesize_sub(
        &self,
        config: &Self::Config,
        challenges: &Challenges<Value<F>>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        config.fp_config.range.load_lookup_table(layouter)?;
        self.assign(layouter, config, challenges)?;
        Ok(())
    }

    fn min_num_rows_block(block: &Block<F>) -> (usize, usize) {
        // EccCircuit can't determine usable rows independently.
        // Instead, the blinding area is determined by other advise columns with most counts of
        // rotation queries. This value is typically determined by either the Keccak or EVM
        // circuit.

        let max_blinding_factor = Self::unusable_rows() - 1;

        // same formula as halo2-lib's FlexGate
        let row_num = (1 << LOG_TOTAL_NUM_ROWS) - (max_blinding_factor + 3);

        let ec_adds = block.get_ec_add_ops().len();
        let ec_muls = block.get_ec_mul_ops().len();
        let ec_pairings = block.get_ec_pairing_ops().len();

        // Instead of showing actual minimum row usage,
        // halo2-lib based circuits use min_row_num to represent a percentage of total-used capacity
        // This functionality allows l2geth to decide if additional ops can be added.
        let min_row_num = [
            (row_num / block.circuits_params.max_ec_ops.ec_add) * ec_adds,
            (row_num / block.circuits_params.max_ec_ops.ec_mul) * ec_muls,
            (row_num / block.circuits_params.max_ec_ops.ec_pairing) * ec_pairings,
        ]
        .into_iter()
        .max()
        .unwrap();

        (min_row_num, row_num)
    }
}
