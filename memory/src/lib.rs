#![no_std]

extern crate alloc;

use crate::columns::{MemoryCols, MEM_COL_MAP, NUM_MEM_COLS};
use alloc::collections::BTreeMap;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::transmute;
use p3_air::VirtualPairCol;
use p3_field::{AbstractField, Field, PrimeField, PrimeField32, PrimeField64};
use p3_matrix::dense::RowMajorMatrix;
use p3_mersenne_31::Mersenne31 as Fp;
use valida_bus::MachineWithMemBus;
use valida_machine::chip::Interaction;
use valida_machine::{Chip, Machine, Word};

pub mod columns;
mod stark;

#[derive(Copy, Clone)]
pub enum Operation {
    Read(Fp, Word<Fp>),
    Write(Fp, Word<Fp>),
    DummyRead(Fp, Word<Fp>),
}

impl Operation {
    pub fn get_address(&self) -> Fp {
        match self {
            Operation::Read(addr, _) => *addr,
            Operation::Write(addr, _) => *addr,
            Operation::DummyRead(addr, _) => *addr,
        }
    }
    pub fn get_value(&self) -> Word<Fp> {
        match self {
            Operation::Read(_, value) => *value,
            Operation::Write(_, value) => *value,
            Operation::DummyRead(_, value) => *value,
        }
    }
}

#[derive(Default)]
pub struct MemoryChip {
    pub cells: BTreeMap<Fp, Word<Fp>>,
    pub operations: BTreeMap<Fp, Vec<Operation>>,
}

pub trait MachineWithMemoryChip: Machine {
    fn mem(&self) -> &MemoryChip;
    fn mem_mut(&mut self) -> &mut MemoryChip;
}

impl MemoryChip {
    pub fn new() -> Self {
        Self {
            cells: BTreeMap::new(),
            operations: BTreeMap::new(),
        }
    }

    pub fn read<A: Into<Fp> + Copy>(&mut self, clk: Fp, address: A, log: bool) -> Word<Fp> {
        let value = self.cells.get(&address.into()).copied().unwrap();
        if log {
            self.operations
                .entry(clk)
                .or_insert_with(Vec::new)
                .push(Operation::Read(address.into(), value));
        }
        value
    }

    pub fn write<V: Into<Word<Fp>> + Copy>(&mut self, clk: Fp, address: Fp, value: V, log: bool) {
        if log {
            self.operations
                .entry(clk)
                .or_insert_with(Vec::new)
                .push(Operation::Write(address, value.into()));
        }
        self.cells.insert(address, value.into());
    }
}

impl<M> Chip<M> for MemoryChip
where
    M: MachineWithMemoryChip + MachineWithMemBus,
{
    type F = Fp;
    type FE = Fp; // FIXME

    fn generate_trace(&self, machine: &M) -> RowMajorMatrix<Self::F> {
        let mut ops = self
            .operations
            .iter()
            .flat_map(|(clk, ops)| {
                ops.iter()
                    .map(|op| (*clk, *op))
                    .collect::<Vec<(Fp, Operation)>>()
            })
            .collect::<Vec<_>>();

        // Sort first by addr, then by clk
        ops.sort_by_key(|(clk, op)| (op.get_address(), *clk));

        // Ensure consecutive sorted clock cycles for an address differ no more than
        // the length of the table (which is capped at 2^29)
        Self::insert_dummy_reads(&mut ops);

        let mut rows = ops
            .into_iter()
            .enumerate()
            .map(|(n, (clk, op))| self.op_to_row(n, clk.as_canonical_u64() as usize, op, machine))
            .collect::<Vec<_>>();

        // Compute address difference values
        Self::compute_address_diffs(&mut rows);

        RowMajorMatrix::new(rows.concat(), NUM_MEM_COLS)
    }

    fn global_receives(&self, machine: &M) -> Vec<Interaction<M::F>> {
        let is_read: VirtualPairCol<M::F> = VirtualPairCol::single_main(MEM_COL_MAP.is_read);
        let addr = VirtualPairCol::single_main(MEM_COL_MAP.addr);
        let value = MEM_COL_MAP.value.0.map(VirtualPairCol::single_main);

        let mut fields = vec![is_read, addr];
        fields.extend(value);

        let receive = Interaction {
            fields: vec![],
            count: VirtualPairCol::single_main(MEM_COL_MAP.is_real),
            argument_index: machine.mem_bus(),
        };
        vec![receive]
    }
}

impl MemoryChip {
    fn op_to_row<M>(&self, n: usize, clk: usize, op: Operation, _machine: &M) -> [Fp; NUM_MEM_COLS]
    where
        M: MachineWithMemoryChip,
    {
        let mut cols = MemoryCols::<Fp>::default();
        cols.clk = Fp::from_canonical_usize(clk);
        cols.counter = Fp::from_canonical_usize(n);

        match op {
            Operation::Read(addr, value) => {
                cols.is_read = Fp::ONE;
                cols.addr = addr;
                cols.value = value;
                cols.is_read = Fp::ONE;
            }
            Operation::Write(addr, value) => {
                cols.addr = addr;
                cols.value = value;
                cols.is_read = Fp::ONE;
            }
            Operation::DummyRead(addr, value) => {
                cols.addr = addr;
                cols.value = value;
            }
        }

        let row: [Fp; NUM_MEM_COLS] = unsafe { transmute(cols) };
        row
    }

    fn insert_dummy_reads(ops: &mut Vec<(Fp, Operation)>) {
        let table_len = ops.len() as u32;
        let mut dummy_ops = Vec::new();
        for (op1, op2) in ops.iter().zip(ops.iter().skip(1)) {
            let addr_diff = op2.1.get_address() - op1.1.get_address();
            if addr_diff != Fp::ZERO {
                continue;
            }
            let clk_diff = (op2.0 - op1.0).as_canonical_u32();
            if clk_diff > table_len {
                let num_dummy_ops = clk_diff / table_len;
                for j in 0..num_dummy_ops {
                    let dummy_op_clk =
                        op1.0 + Fp::from_canonical_u32(table_len) * Fp::from_canonical_u32(j + 1);
                    let dummy_op_addr = op1.1.get_address();
                    let dummy_op_value = op1.1.get_value();
                    dummy_ops.push((
                        dummy_op_clk,
                        Operation::DummyRead(dummy_op_addr, dummy_op_value),
                    ));
                }
            }
        }
        // TODO: Track number of operations at a given address instead of recounting here
        for (clk, op) in dummy_ops.iter() {
            let idx_addr = ops
                .binary_search_by_key(&op.get_address(), |(_, op)| op.get_address())
                .unwrap();
            let num_ops = ops[idx_addr..]
                .iter()
                .take_while(|(_, op2)| op.get_address() == op2.get_address())
                .count();
            let idx_clk =
                ops[idx_addr..(idx_addr + num_ops)].partition_point(|(clk2, _)| clk2 < clk);
            ops.insert(idx_addr + idx_clk, (*clk, *op));
        }
    }

    fn compute_address_diffs(rows: &mut Vec<[Fp; NUM_MEM_COLS]>) {
        // TODO: Use batch inversion
        for n in 0..(rows.len() - 1) {
            let addr = rows[n][MEM_COL_MAP.addr];
            let addr_next = rows[n][MEM_COL_MAP.addr];
            rows[n][MEM_COL_MAP.diff] = addr_next - addr;
            if (addr - addr_next) != Fp::ZERO {
                rows[n][MEM_COL_MAP.diff_inv] = (addr_next - addr).try_inverse().unwrap();
                rows[n][MEM_COL_MAP.addr_not_equal] = Fp::ONE;
            }
        }
    }
}
