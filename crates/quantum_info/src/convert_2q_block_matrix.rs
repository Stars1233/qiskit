// This code is part of Qiskit.
//
// (C) Copyright IBM 2022
//
// This code is licensed under the Apache License, Version 2.0. You may
// obtain a copy of this license in the LICENSE.txt file in the root directory
// of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
//
// Any modifications or derivative works of this code must retain this
// copyright notice, and modified files need to carry a notice indicating
// that they have been altered from the originals.

use pyo3::intern;
use pyo3::prelude::*;
use pyo3::Python;

use num_complex::Complex64;
use numpy::ndarray::{arr2, aview2, Array2, ArrayView2, ArrayViewMut2};
use numpy::PyReadonlyArray2;
use rustworkx_core::petgraph::stable_graph::NodeIndex;

use qiskit_circuit::dag_circuit::DAGCircuit;
use qiskit_circuit::gate_matrix::TWO_QUBIT_IDENTITY;
use qiskit_circuit::imports::QI_OPERATOR;
use qiskit_circuit::operations::{ArrayType, Operation, OperationRef};
use qiskit_circuit::packed_instruction::PackedInstruction;
use qiskit_circuit::Qubit;

use crate::versor_u2::{VersorSU2, VersorU2, VersorU2Error};
use crate::QiskitError;

#[inline]
pub fn get_matrix_from_inst(inst: &PackedInstruction) -> PyResult<Array2<Complex64>> {
    if let Some(mat) = inst.op.matrix(inst.params_view()) {
        Ok(mat)
    } else if inst.op.try_standard_gate().is_some() {
        Err(QiskitError::new_err(
            "Parameterized gates can't be consolidated",
        ))
    } else if let OperationRef::Gate(gate) = inst.op.view() {
        // If the operation is a custom python gate, we will acquire the gil
        // and use an Operator. Otherwise, using op.matrix() should work.
        // A user should not be able to reach this condition in Rust standalone
        // mode.
        Python::with_gil(|py| -> PyResult<_> {
            Ok(QI_OPERATOR
                .get_bound(py)
                .call1((gate.gate.clone_ref(py),))?
                .getattr(intern!(py, "data"))?
                .extract::<PyReadonlyArray2<Complex64>>()?
                .as_array()
                .to_owned())
        })
    } else {
        Err(QiskitError::new_err(
            "Can't compute matrix of non-unitary op",
        ))
    }
}

/// Quaternion-based collect of two parallel runs of 1q gates.
#[derive(Clone, Debug)]
struct Separable1q {
    phase: f64,
    qubits: [VersorSU2; 2],
}
impl Separable1q {
    /// Construct an initial state from a single qubit operation.
    #[inline]
    fn from_qubit(n: usize, versor: VersorU2) -> Self {
        let mut qubits: [VersorSU2; 2] = Default::default();
        qubits[n] = versor.su2;
        Self {
            phase: versor.phase,
            qubits,
        }
    }

    /// Apply a new gate to one of the two qubits.
    #[inline]
    fn apply_on_qubit(&mut self, n: usize, versor: &VersorU2) {
        self.phase += versor.phase;
        self.qubits[n] = versor.su2 * self.qubits[n];
    }

    /// Construct the two-qubit gate matrix implied by these two runs.
    #[inline]
    fn matrix_into<'a>(&self, target: &'a mut [[Complex64; 4]; 4]) -> ArrayView2<'a, Complex64> {
        let q0 = VersorU2 {
            phase: self.phase,
            su2: self.qubits[0],
        }
        .matrix_contiguous();

        // This is the manually unrolled Kronecker product.
        let [iz, iy, ix, scalar] = *self.qubits[1].0.quaternion().coords.as_ref();
        let q1_row = [Complex64::new(scalar, iz), Complex64::new(iy, ix)];
        for out_row in 0..2 {
            for out_col in 0..4 {
                target[out_row][out_col] = q1_row[(out_col & 2) >> 1] * q0[out_row][out_col & 1];
            }
        }
        let q1_row = [Complex64::new(-iy, ix), Complex64::new(scalar, -iz)];
        for out_row in 0..2 {
            for out_col in 0..4 {
                target[out_row + 2][out_col] =
                    q1_row[(out_col & 2) >> 1] * q0[out_row][out_col & 1];
            }
        }
        aview2(target)
    }
}

/// Extract a versor representation of an arbitrary 1q DAG instruction.
fn versor_from_1q_gate(inst: &PackedInstruction) -> PyResult<VersorU2> {
    let tol = 1e-12;
    match inst.op.view() {
        OperationRef::StandardGate(gate) => VersorU2::from_standard(gate, inst.params_view()),
        OperationRef::Unitary(gate) => match &gate.array {
            ArrayType::NDArray(arr) => Ok(VersorU2::from_ndarray_unchecked(&arr.view())),
            ArrayType::OneQ(arr) => Ok(VersorU2::from_nalgebra_unchecked(arr)),
            ArrayType::TwoQ(_) => Err(VersorU2Error::MultiQubit),
        },
        _ => VersorU2::from_ndarray(&get_matrix_from_inst(inst)?.view(), tol),
    }
    .map_err(|err| QiskitError::new_err(err.to_string()))
}

/// Return the matrix Operator resulting from a block of Instructions.
///
/// # Panics
///
/// If any node in `op_list` is not a 1q or 2q gate.
pub fn blocks_to_matrix(
    dag: &DAGCircuit,
    op_list: &[NodeIndex],
    block_index_map: [Qubit; 2],
) -> PyResult<Array2<Complex64>> {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Qarg {
        Q0 = 0,
        Q1 = 1,
        Q01 = 2,
        Q10 = 3,
    }
    let qarg_vals = [Qarg::Q0, Qarg::Q1, Qarg::Q01, Qarg::Q10];
    let interned_default = dag.qargs_interner().get_default();
    let interned_map = [
        &[block_index_map[0]],
        &[block_index_map[1]],
        block_index_map.as_slice(),
        &[block_index_map[1], block_index_map[0]],
    ]
    .map(|qubits| {
        dag.qargs_interner()
            .try_key(qubits)
            .unwrap_or(interned_default)
    });
    let qarg_lookup = |qargs| {
        qarg_vals[interned_map
            .iter()
            .position(|key| qargs == *key)
            .expect("not a 1q or 2q gate in the qargs block")]
    };

    let mut work: [[Complex64; 4]; 4] = Default::default();
    let mut qubits_1q: Option<Separable1q> = None;
    let mut output_matrix: Option<Array2<Complex64>> = None;
    for node in op_list {
        let inst = dag[*node].unwrap_operation();
        let qarg = qarg_lookup(inst.qubits);
        match qarg {
            Qarg::Q0 | Qarg::Q1 => {
                let versor = versor_from_1q_gate(inst)?;
                match qubits_1q.as_mut() {
                    Some(sep) => sep.apply_on_qubit(qarg as usize, &versor),
                    None => qubits_1q = Some(Separable1q::from_qubit(qarg as usize, versor)),
                };
            }
            Qarg::Q01 | Qarg::Q10 => {
                let mut matrix = get_matrix_from_inst(inst)?;
                if qarg == Qarg::Q10 {
                    change_basis_inplace(matrix.view_mut());
                }
                if let Some(sep) = qubits_1q.take() {
                    matrix = matrix.dot(&sep.matrix_into(&mut work));
                }
                output_matrix = if let Some(state) = output_matrix {
                    Some(matrix.dot(&state))
                } else {
                    Some(matrix)
                };
            }
        }
    }
    match (
        qubits_1q.map(|sep| sep.matrix_into(&mut work)),
        output_matrix,
    ) {
        (Some(sep), Some(state)) => Ok(sep.dot(&state)),
        (None, Some(state)) => Ok(state),
        (Some(sep), None) => Ok(sep.to_owned()),
        // This shouldn't actually ever trigger, because we expect blocks to be non-empty, but it's
        // trivial to handle anyway.
        (None, None) => Ok(arr2(&TWO_QUBIT_IDENTITY)),
    }
}

/// Switches the order of qubits in a two qubit operation.
#[inline]
pub fn change_basis(matrix: ArrayView2<Complex64>) -> Array2<Complex64> {
    let mut trans_matrix: Array2<Complex64> = matrix.reversed_axes().to_owned();
    for index in 0..trans_matrix.ncols() {
        trans_matrix.swap([1, index], [2, index]);
    }
    trans_matrix = trans_matrix.reversed_axes();
    for index in 0..trans_matrix.ncols() {
        trans_matrix.swap([1, index], [2, index]);
    }
    trans_matrix
}

/// Change the qubit order of a 2q matrix in place.
#[inline]
pub fn change_basis_inplace(mut matrix: ArrayViewMut2<Complex64>) {
    matrix.swap((0, 1), (0, 2));
    matrix.swap((3, 1), (3, 2));
    matrix.swap((1, 0), (2, 0));
    matrix.swap((1, 3), (2, 3));
    matrix.swap((1, 1), (2, 2));
    matrix.swap((1, 2), (2, 1));
}
