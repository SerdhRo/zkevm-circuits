//! The EVM circuit implementation.

#![allow(missing_docs)]
use halo2_proofs::{circuit::Layouter, plonk::*};

mod execution;
pub mod param;
mod step;
pub(crate) mod util;

pub mod table;
pub mod witness;

use crate::bytecode_circuit::bytecode_unroller::BytecodeTable;
use crate::tx_circuit::TxTable;
use crate::{
    evm_circuit::{
        util::{rlc, RandomLinearCombination},
        witness::{BlockContext, Bytecode, RwMap, Transaction},
    },
    rw_table::RwTable,
};
use eth_types::{Field, ToLittleEndian, Word};
use execution::ExecutionConfig;
use itertools::Itertools;
use keccak256::plain::Keccak;
use table::{BlockTable, FixedTableTag, KeccakTable, LookupTable, TableColumns};
use witness::Block;

/// EvmCircuit implements verification of execution trace of a block.
#[derive(Clone, Debug)]
pub struct EvmCircuit<F> {
    fixed_table: [Column<Fixed>; 4],
    byte_table: [Column<Fixed>; 1],
    execution: Box<ExecutionConfig<F>>,
}

impl<F: Field> EvmCircuit<F> {
    /// Configure EvmCircuit
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        power_of_randomness: [Expression<F>; 31],
        tx_table: &dyn LookupTable<F>,
        rw_table: &dyn LookupTable<F>,
        bytecode_table: &dyn LookupTable<F>,
        block_table: &dyn LookupTable<F>,
    ) -> Self {
        let fixed_table = [(); 4].map(|_| meta.fixed_column());
        let byte_table = [(); 1].map(|_| meta.fixed_column());

        let execution = Box::new(ExecutionConfig::configure(
            meta,
            power_of_randomness,
            &fixed_table,
            &byte_table,
            tx_table,
            rw_table,
            bytecode_table,
            block_table,
        ));

        Self {
            fixed_table,
            byte_table,
            execution,
        }
    }

    /// Load fixed table
    pub fn load_fixed_table(
        &self,
        layouter: &mut impl Layouter<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    ) -> Result<(), Error> {
        layouter.assign_region(
            || "fixed table",
            |mut region| {
                for (offset, row) in std::iter::once([F::zero(); 4])
                    .chain(fixed_table_tags.iter().flat_map(|tag| tag.build()))
                    .enumerate()
                {
                    for (column, value) in self.fixed_table.iter().zip_eq(row) {
                        region.assign_fixed(|| "", *column, offset, || Ok(value))?;
                    }
                }

                Ok(())
            },
        )
    }

    /// Load byte table
    pub fn load_byte_table(&self, layouter: &mut impl Layouter<F>) -> Result<(), Error> {
        layouter.assign_region(
            || "byte table",
            |mut region| {
                for offset in 0..256 {
                    region.assign_fixed(
                        || "",
                        self.byte_table[0],
                        offset,
                        || Ok(F::from(offset as u64)),
                    )?;
                }

                Ok(())
            },
        )
    }

    /// Assign block
    pub fn assign_block(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
    ) -> Result<(), Error> {
        self.execution.assign_block(layouter, block, false)
    }

    /// Assign exact steps in block without padding for unit test purpose
    #[cfg(any(feature = "test", test))]
    pub fn assign_block_exact(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
    ) -> Result<(), Error> {
        self.execution.assign_block(layouter, block, true)
    }

    /// Calculate which rows are "actually" used in the circuit
    pub fn get_active_rows(&self, block: &Block<F>) -> (Vec<usize>, Vec<usize>) {
        let max_offset = self.get_num_rows_required(block);
        // some gates are enabled on all rows
        let gates_row_ids = (0..max_offset).collect();
        // lookups are enabled at "q_step" rows and byte lookup rows
        let lookup_row_ids = (0..max_offset).collect();
        (gates_row_ids, lookup_row_ids)
    }

    pub fn get_num_rows_required(&self, block: &Block<F>) -> usize {
        // Start at 1 so we can be sure there is an unused `next` row available
        let mut num_rows = 1;
        for transaction in &block.txs {
            for step in &transaction.steps {
                num_rows += self.execution.get_step_height(step.execution_state);
            }
        }
        num_rows
    }
}

// TODO: Move to src/tables.rs
pub fn load_txs<F: Field>(
    tx_table: &TxTable,
    layouter: &mut impl Layouter<F>,
    txs: &[Transaction],
    randomness: F,
) -> Result<(), Error> {
    layouter.assign_region(
        || "tx table",
        |mut region| {
            let mut offset = 0;
            for column in tx_table.columns() {
                region.assign_advice(
                    || "tx table all-zero row",
                    column,
                    offset,
                    || Ok(F::zero()),
                )?;
            }
            offset += 1;

            // println!("DBG load_txs");
            let tx_table_columns = tx_table.columns();
            for tx in txs.iter() {
                for row in tx.table_assignments(randomness) {
                    // print!("{:02} ", offset);
                    for (column, value) in tx_table_columns.iter().zip_eq(row) {
                        // print!("{:?} ", value);
                        region.assign_advice(
                            || format!("tx table row {}", offset),
                            *column,
                            offset,
                            || Ok(value),
                        )?;
                    }
                    offset += 1;
                    // println!("");
                }
            }
            Ok(())
        },
    )
}

// TODO: Move to src/tables.rs
pub fn load_rws<F: Field>(
    rw_table: &RwTable,
    layouter: &mut impl Layouter<F>,
    rws: &RwMap,
    randomness: F,
) -> Result<(), Error> {
    layouter.assign_region(
        || "rw table",
        |mut region| {
            let mut offset = 0;
            rw_table.assign(&mut region, offset, &Default::default())?;
            offset += 1;

            let mut rows = rws
                .0
                .values()
                .flat_map(|rws| rws.iter())
                .collect::<Vec<_>>();

            rows.sort_by_key(|a| a.rw_counter());
            let mut expected_rw_counter = 1;
            for rw in rows {
                assert!(rw.rw_counter() == expected_rw_counter);
                expected_rw_counter += 1;

                rw_table.assign(&mut region, offset, &rw.table_assignment(randomness))?;
                offset += 1;
            }
            Ok(())
        },
    )
}

// use crate::util::TableShow;

// TODO: Move to src/tables.rs
pub fn load_bytecodes<'a, F: Field>(
    bytecode_table: &BytecodeTable,
    layouter: &mut impl Layouter<F>,
    bytecodes: impl IntoIterator<Item = &'a Bytecode> + Clone,
    randomness: F,
) -> Result<(), Error> {
    // println!("> load_bytecodes");
    // let mut table = TableShow::<F>::new(vec!["codeHash", "tag", "index",
    // "isCode", "value"]);
    layouter.assign_region(
        || "bytecode table",
        |mut region| {
            let mut offset = 0;
            for column in bytecode_table.columns() {
                region.assign_advice(
                    || "bytecode table all-zero row",
                    column,
                    offset,
                    || Ok(F::zero()),
                )?;
            }
            offset += 1;

            let bytecode_table_columns = bytecode_table.columns();
            for bytecode in bytecodes.clone() {
                for row in bytecode.table_assignments(randomness) {
                    // let mut column_index = 0;
                    for (column, value) in bytecode_table_columns.iter().zip_eq(row) {
                        region.assign_advice(
                            || format!("bytecode table row {}", offset),
                            *column,
                            offset,
                            || Ok(value),
                        )?;
                        // table.push(column_index, value);
                        // column_index += 1;
                    }
                    offset += 1;
                }
            }
            // table.print();
            Ok(())
        },
    )
}

// TODO: Move to src/tables.rs
pub fn load_block<F: Field>(
    block_table: &BlockTable,
    layouter: &mut impl Layouter<F>,
    block: &BlockContext,
    randomness: F,
) -> Result<(), Error> {
    layouter.assign_region(
        || "block table",
        |mut region| {
            let mut offset = 0;
            for column in block_table.columns() {
                region.assign_advice(
                    || "block table all-zero row",
                    column,
                    offset,
                    || Ok(F::zero()),
                )?;
            }
            offset += 1;

            let block_table_columns = block_table.columns();
            for row in block.table_assignments(randomness) {
                for (column, value) in block_table_columns.iter().zip_eq(row) {
                    region.assign_advice(
                        || format!("block table row {}", offset),
                        *column,
                        offset,
                        || Ok(value),
                    )?;
                }
                offset += 1;
            }

            Ok(())
        },
    )
}

pub fn keccak_table_assignments<F: Field>(input: &[u8], randomness: F) -> Vec<[F; 4]> {
    // CHANGELOG: Using `RLC(reversed(input))`
    let input_rlc: F = rlc::value(input.iter().rev(), randomness);
    let input_len = F::from(input.len() as u64);
    let mut keccak = Keccak::default();
    keccak.update(input);
    let output = keccak.digest();
    let output_rlc = RandomLinearCombination::<F, 32>::random_linear_combine(
        Word::from_big_endian(output.as_slice()).to_le_bytes(),
        randomness,
    );

    vec![[F::one(), input_rlc, input_len, output_rlc]]
}

// NOTE: For now, the input_rlc of the keccak is defined as
// `RLC(reversed(input))` for convenience of the circuits that do the lookups.
// This allows calculating the `input_rlc` after all the inputs bytes have been
// layed out via the pattern `acc[i] = acc[i-1] * r + value[i]`.
pub fn load_keccaks<'a, F: Field>(
    keccak_table: &KeccakTable,
    layouter: &mut impl Layouter<F>,
    inputs: impl IntoIterator<Item = &'a [u8]> + Clone,
    randomness: F,
) -> Result<(), Error> {
    // println!("> super_circuit.load_keccaks");
    // let mut table = TableShow::<F>::new(vec!["is_enabled", "input_rlc",
    // "input_len", "output_rlc"]);
    layouter.assign_region(
        || "keccak table",
        |mut region| {
            let mut offset = 0;
            for column in keccak_table.columns() {
                region.assign_advice(
                    || "keccak table all-zero row",
                    column,
                    offset,
                    || Ok(F::zero()),
                )?;
            }
            offset += 1;

            let keccak_table_columns = keccak_table.columns();
            for input in inputs.clone() {
                // println!("+ {:?}", input);
                for row in keccak_table_assignments(input, randomness) {
                    // let mut column_index = 0;
                    for (column, value) in keccak_table_columns.iter().zip_eq(row) {
                        region.assign_advice(
                            || format!("keccak table row {}", offset),
                            *column,
                            offset,
                            || Ok(value),
                        )?;
                        // table.push(column_index, value);
                        // column_index += 1;
                    }
                    offset += 1;
                }
            }
            // table.print();
            Ok(())
        },
    )
}

#[cfg(any(feature = "test", test))]
pub mod test {
    use super::*;
    use crate::{
        evm_circuit::{table::FixedTableTag, witness::Block, EvmCircuit},
        rw_table::RwTable,
        util::power_of_randomness_from_instance,
    };
    use eth_types::{Field, Word};
    use halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner},
        dev::{MockProver, VerifyFailure},
        plonk::{Circuit, ConstraintSystem, Error},
    };
    use rand::{
        distributions::uniform::{SampleRange, SampleUniform},
        random, thread_rng, Rng,
    };
    use strum::IntoEnumIterator;

    pub(crate) fn rand_range<T, R>(range: R) -> T
    where
        T: SampleUniform,
        R: SampleRange<T>,
    {
        thread_rng().gen_range(range)
    }

    pub(crate) fn rand_bytes(n: usize) -> Vec<u8> {
        (0..n).map(|_| random()).collect()
    }

    pub(crate) fn rand_bytes_array<const N: usize>() -> [u8; N] {
        [(); N].map(|_| random())
    }

    pub(crate) fn rand_word() -> Word {
        Word::from_big_endian(&rand_bytes_array::<32>())
    }

    #[derive(Clone)]
    pub struct TestCircuitConfig<F> {
        tx_table: TxTable,
        rw_table: RwTable,
        bytecode_table: BytecodeTable,
        block_table: BlockTable,
        evm_circuit: EvmCircuit<F>,
    }

    #[derive(Default)]
    pub struct TestCircuit<F> {
        block: Block<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    }

    impl<F> TestCircuit<F> {
        pub fn new(block: Block<F>, fixed_table_tags: Vec<FixedTableTag>) -> Self {
            Self {
                block,
                fixed_table_tags,
            }
        }
    }

    impl<F: Field> Circuit<F> for TestCircuit<F> {
        type Config = TestCircuitConfig<F>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let tx_table = TxTable::construct(meta);
            let rw_table = RwTable::construct(meta);
            let bytecode_table = BytecodeTable::construct(meta);
            let block_table = BlockTable::construct(meta);

            let power_of_randomness = power_of_randomness_from_instance(meta);
            let evm_circuit = EvmCircuit::configure(
                meta,
                power_of_randomness,
                &tx_table,
                &rw_table,
                &bytecode_table,
                &block_table,
            );

            Self::Config {
                tx_table,
                rw_table,
                bytecode_table,
                block_table,
                evm_circuit,
            }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), Error> {
            config
                .evm_circuit
                .load_fixed_table(&mut layouter, self.fixed_table_tags.clone())?;
            config.evm_circuit.load_byte_table(&mut layouter)?;
            load_txs(
                &config.tx_table,
                &mut layouter,
                &self.block.txs,
                self.block.randomness,
            )?;
            load_rws(
                &config.rw_table,
                &mut layouter,
                &self.block.rws,
                self.block.randomness,
            )?;
            load_bytecodes(
                &config.bytecode_table,
                &mut layouter,
                self.block.bytecodes.iter().map(|(_, b)| b),
                self.block.randomness,
            )?;
            load_block(
                &config.block_table,
                &mut layouter,
                &self.block.context,
                self.block.randomness,
            )?;
            config
                .evm_circuit
                .assign_block_exact(&mut layouter, &self.block)
        }
    }

    impl<F: Field> TestCircuit<F> {
        pub fn get_num_rows_required(block: &Block<F>) -> usize {
            let mut cs = ConstraintSystem::default();
            let config = TestCircuit::configure(&mut cs);
            config.evm_circuit.get_num_rows_required(block)
        }

        pub fn get_active_rows(block: &Block<F>) -> (Vec<usize>, Vec<usize>) {
            let mut cs = ConstraintSystem::default();
            let config = TestCircuit::configure(&mut cs);
            config.evm_circuit.get_active_rows(block)
        }
    }

    pub fn run_test_circuit<F: Field>(
        block: Block<F>,
        fixed_table_tags: Vec<FixedTableTag>,
    ) -> Result<(), Vec<VerifyFailure>> {
        let log2_ceil = |n| u32::BITS - (n as u32).leading_zeros() - (n & (n - 1) == 0) as u32;

        let num_rows_required_for_steps = TestCircuit::get_num_rows_required(&block);

        let k = log2_ceil(
            64 + fixed_table_tags
                .iter()
                .map(|tag| tag.build::<F>().count())
                .sum::<usize>(),
        );
        let k = k.max(log2_ceil(
            64 + block
                .bytecodes
                .values()
                .map(|bytecode| bytecode.bytes.len())
                .sum::<usize>(),
        ));
        let k = k.max(log2_ceil(64 + num_rows_required_for_steps));
        log::debug!("evm circuit uses k = {}", k);

        let power_of_randomness = (1..32)
            .map(|exp| vec![block.randomness.pow(&[exp, 0, 0, 0]); (1 << k) - 64])
            .collect();
        let (active_gate_rows, active_lookup_rows) = TestCircuit::get_active_rows(&block);
        let circuit = TestCircuit::<F>::new(block, fixed_table_tags);
        let prover = MockProver::<F>::run(k, &circuit, power_of_randomness).unwrap();
        prover.verify_at_rows(active_gate_rows.into_iter(), active_lookup_rows.into_iter())
    }

    pub fn run_test_circuit_incomplete_fixed_table<F: Field>(
        block: Block<F>,
    ) -> Result<(), Vec<VerifyFailure>> {
        run_test_circuit(
            block,
            vec![
                FixedTableTag::Zero,
                FixedTableTag::Range5,
                FixedTableTag::Range16,
                FixedTableTag::Range32,
                FixedTableTag::Range64,
                FixedTableTag::Range256,
                FixedTableTag::Range512,
                FixedTableTag::Range1024,
                FixedTableTag::SignByte,
                FixedTableTag::ResponsibleOpcode,
                FixedTableTag::Pow2,
            ],
        )
    }

    pub fn run_test_circuit_complete_fixed_table<F: Field>(
        block: Block<F>,
    ) -> Result<(), Vec<VerifyFailure>> {
        run_test_circuit(block, FixedTableTag::iter().collect())
    }
}
