//! This crate provides support for cross-table lookups.
//!
//! If a STARK S_1 calls an operation that is carried out by another STARK S_2,
//! S_1 provides the inputs to S_2 and reads the output from S_1. To ensure that
//! the operation was correctly carried out, we must check that the provided inputs
//! and outputs are correctly read. Cross-table lookups carry out that check.
//!
//! To achieve this, smaller CTL tables are created on both sides: looking and looked tables.
//! In our example, we create a table S_1' comprised of columns -- or linear combinations
//! of columns -- of S_1, and rows that call operations carried out in S_2. We also create a
//! table S_2' comprised of columns -- or linear combinations od columns -- of S_2 and rows
//! that carry out the operations needed by other STARKs. Then, S_1' is a looking table for
//! the looked S_2', since we want to check that the operation outputs in S_1' are indeeed in S_2'.
//! Furthermore, the concatenation of all tables looking into S_2' must be equal to S_2'.
//!
//! To achieve this, we construct, for each table, a permutation polynomial Z(x).
//! Z(x) is computed as the product of all its column combinations.
//! To check it was correctly constructed, we check:
//! - Z(gw) = Z(w) * combine(w) where combine(w) is the column combination at point w.
//! - Z(g^(n-1)) = combine(1).
//! - The verifier also checks that the product of looking table Z polynomials is equal
//! to the associated looked table Z polynomial.
//! Note that the first two checks are written that way because Z polynomials are computed
//! upside down for convenience.
//!
//! Additionally, we support cross-table lookups over two rows. The permutation principle
//! is similar, but we provide not only `local_values` but also `next_values` -- corresponding to
//! the current and next row values -- when computing the linear combinations.

use std::borrow::Borrow;
use std::fmt::Debug;
use std::iter::repeat;

use anyhow::{ensure, Result};
use itertools::Itertools;
use plonky2::field::extension::{Extendable, FieldExtension};
use plonky2::field::packed::PackedField;
use plonky2::field::polynomial::PolynomialValues;
use plonky2::field::types::Field;
use plonky2::hash::hash_types::RichField;
use plonky2::iop::challenger::{Challenger, RecursiveChallenger};
use plonky2::iop::ext_target::ExtensionTarget;
use plonky2::iop::target::Target;
use plonky2::plonk::circuit_builder::CircuitBuilder;
use plonky2::plonk::config::{AlgebraicHasher, GenericConfig, Hasher};
use plonky2::plonk::plonk_common::{
    reduce_with_powers, reduce_with_powers_circuit, reduce_with_powers_ext_circuit,
};
use plonky2::util::serialization::{Buffer, IoResult, Read, Write};

use crate::all_stark::{Table, NUM_TABLES};
use crate::config::StarkConfig;
use crate::constraint_consumer::{ConstraintConsumer, RecursiveConstraintConsumer};
use crate::evaluation_frame::StarkEvaluationFrame;
use crate::proof::{StarkProofTarget, StarkProofWithMetadata};
use crate::stark::Stark;

/// Represent two linear combination of columns, corresponding to the current and next row values.
/// Each linear combination is represented as:
/// - a vector of `(usize, F)` corresponding to the column number and the associated multiplicand
/// - the constant of the linear combination.
#[derive(Clone, Debug)]
pub struct Column<F: Field> {
    linear_combination: Vec<(usize, F)>,
    next_row_linear_combination: Vec<(usize, F)>,
    constant: F,
}

impl<F: Field> Column<F> {
    /// Returns the representation of a single column in the current row.
    pub fn single(c: usize) -> Self {
        Self {
            linear_combination: vec![(c, F::ONE)],
            next_row_linear_combination: vec![],
            constant: F::ZERO,
        }
    }

    /// Returns multiple single columns in the current row.
    pub fn singles<I: IntoIterator<Item = impl Borrow<usize>>>(
        cs: I,
    ) -> impl Iterator<Item = Self> {
        cs.into_iter().map(|c| Self::single(*c.borrow()))
    }

    /// Returns the representation of a single column in the next row.
    pub fn single_next_row(c: usize) -> Self {
        Self {
            linear_combination: vec![],
            next_row_linear_combination: vec![(c, F::ONE)],
            constant: F::ZERO,
        }
    }

    /// Returns multiple single columns for the next row.
    pub fn singles_next_row<I: IntoIterator<Item = impl Borrow<usize>>>(
        cs: I,
    ) -> impl Iterator<Item = Self> {
        cs.into_iter().map(|c| Self::single_next_row(*c.borrow()))
    }

    /// Returns a linear combination corresponding to a constant.
    pub fn constant(constant: F) -> Self {
        Self {
            linear_combination: vec![],
            next_row_linear_combination: vec![],
            constant,
        }
    }

    /// Returns a linear combination corresponding to 0.
    pub fn zero() -> Self {
        Self::constant(F::ZERO)
    }

    /// Returns a linear combination corresponding to 1.
    pub fn one() -> Self {
        Self::constant(F::ONE)
    }

    /// Given an iterator of `(usize, F)` and a constant, returns the association linear combination of columns for the current row.
    pub fn linear_combination_with_constant<I: IntoIterator<Item = (usize, F)>>(
        iter: I,
        constant: F,
    ) -> Self {
        let v = iter.into_iter().collect::<Vec<_>>();
        assert!(!v.is_empty());
        debug_assert_eq!(
            v.iter().map(|(c, _)| c).unique().count(),
            v.len(),
            "Duplicate columns."
        );
        Self {
            linear_combination: v,
            next_row_linear_combination: vec![],
            constant,
        }
    }

    /// Given an iterator of `(usize, F)` and a constant, returns the associated linear combination of columns for the current and the next rows.
    pub fn linear_combination_and_next_row_with_constant<I: IntoIterator<Item = (usize, F)>>(
        iter: I,
        next_row_iter: I,
        constant: F,
    ) -> Self {
        let v = iter.into_iter().collect::<Vec<_>>();
        let next_row_v = next_row_iter.into_iter().collect::<Vec<_>>();

        assert!(!v.is_empty() || !next_row_v.is_empty());
        debug_assert_eq!(
            v.iter().map(|(c, _)| c).unique().count(),
            v.len(),
            "Duplicate columns."
        );
        debug_assert_eq!(
            next_row_v.iter().map(|(c, _)| c).unique().count(),
            next_row_v.len(),
            "Duplicate columns."
        );

        Self {
            linear_combination: v,
            next_row_linear_combination: next_row_v,
            constant,
        }
    }

    /// Returns a linear combination of columns, with no additional constant.
    pub fn linear_combination<I: IntoIterator<Item = (usize, F)>>(iter: I) -> Self {
        Self::linear_combination_with_constant(iter, F::ZERO)
    }

    /// Given an iterator of columns (c_0, ..., c_n) containing bits in little endian order:
    /// returns the representation of c_0 + 2 * c_1 + ... + 2^n * c_n.
    pub fn le_bits<I: IntoIterator<Item = impl Borrow<usize>>>(cs: I) -> Self {
        Self::linear_combination(cs.into_iter().map(|c| *c.borrow()).zip(F::TWO.powers()))
    }

    /// Given an iterator of columns (c_0, ..., c_n) containing bytes in little endian order:
    /// returns the representation of c_0 + 256 * c_1 + ... + 256^n * c_n.
    pub fn le_bytes<I: IntoIterator<Item = impl Borrow<usize>>>(cs: I) -> Self {
        Self::linear_combination(
            cs.into_iter()
                .map(|c| *c.borrow())
                .zip(F::from_canonical_u16(256).powers()),
        )
    }

    /// Given an iterator of columns, returns the representation of their sum.
    pub fn sum<I: IntoIterator<Item = impl Borrow<usize>>>(cs: I) -> Self {
        Self::linear_combination(cs.into_iter().map(|c| *c.borrow()).zip(repeat(F::ONE)))
    }

    /// Given the column values for the current row, returns the evaluation of the linear combination.
    pub fn eval<FE, P, const D: usize>(&self, v: &[P]) -> P
    where
        FE: FieldExtension<D, BaseField = F>,
        P: PackedField<Scalar = FE>,
    {
        self.linear_combination
            .iter()
            .map(|&(c, f)| v[c] * FE::from_basefield(f))
            .sum::<P>()
            + FE::from_basefield(self.constant)
    }

    /// Given the column values for the current and next rows, evaluates the current and next linear combinations and returns their sum.
    pub fn eval_with_next<FE, P, const D: usize>(&self, v: &[P], next_v: &[P]) -> P
    where
        FE: FieldExtension<D, BaseField = F>,
        P: PackedField<Scalar = FE>,
    {
        self.linear_combination
            .iter()
            .map(|&(c, f)| v[c] * FE::from_basefield(f))
            .sum::<P>()
            + self
                .next_row_linear_combination
                .iter()
                .map(|&(c, f)| next_v[c] * FE::from_basefield(f))
                .sum::<P>()
            + FE::from_basefield(self.constant)
    }

    /// Evaluate on a row of a table given in column-major form.
    pub fn eval_table(&self, table: &[PolynomialValues<F>], row: usize) -> F {
        let mut res = self
            .linear_combination
            .iter()
            .map(|&(c, f)| table[c].values[row] * f)
            .sum::<F>()
            + self.constant;

        // If we access the next row at the last row, for sanity, we consider the next row's values to be 0.
        // If CTLs are correctly written, the filter should be 0 in that case anyway.
        if !self.next_row_linear_combination.is_empty() && row < table[0].values.len() - 1 {
            res += self
                .next_row_linear_combination
                .iter()
                .map(|&(c, f)| table[c].values[row + 1] * f)
                .sum::<F>();
        }

        res
    }

    /// Circuit version of `eval`: Given a row's targets, returns their linear combination.
    pub fn eval_circuit<const D: usize>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        v: &[ExtensionTarget<D>],
    ) -> ExtensionTarget<D>
    where
        F: RichField + Extendable<D>,
    {
        let pairs = self
            .linear_combination
            .iter()
            .map(|&(c, f)| {
                (
                    v[c],
                    builder.constant_extension(F::Extension::from_basefield(f)),
                )
            })
            .collect::<Vec<_>>();
        let constant = builder.constant_extension(F::Extension::from_basefield(self.constant));
        builder.inner_product_extension(F::ONE, constant, pairs)
    }

    /// Circuit version of `eval_with_next`:
    /// Given the targets of the current and next row, returns the sum of their linear combinations.
    pub fn eval_with_next_circuit<const D: usize>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        v: &[ExtensionTarget<D>],
        next_v: &[ExtensionTarget<D>],
    ) -> ExtensionTarget<D>
    where
        F: RichField + Extendable<D>,
    {
        let mut pairs = self
            .linear_combination
            .iter()
            .map(|&(c, f)| {
                (
                    v[c],
                    builder.constant_extension(F::Extension::from_basefield(f)),
                )
            })
            .collect::<Vec<_>>();
        let next_row_pairs = self.next_row_linear_combination.iter().map(|&(c, f)| {
            (
                next_v[c],
                builder.constant_extension(F::Extension::from_basefield(f)),
            )
        });
        pairs.extend(next_row_pairs);
        let constant = builder.constant_extension(F::Extension::from_basefield(self.constant));
        builder.inner_product_extension(F::ONE, constant, pairs)
    }
}

/// A `Table` with a linear combination of columns and a filter.
/// `filter_column` is used to determine the rows to select in `Table`.
/// `columns` represents linear combinations of the columns of `Table`.
#[derive(Clone, Debug)]
pub struct TableWithColumns<F: Field> {
    table: Table,
    columns: Vec<Column<F>>,
    pub(crate) filter_column: Option<Column<F>>,
}

impl<F: Field> TableWithColumns<F> {
    /// Generates a new `TableWithColumns` given a `Table`, a linear combination of columns `columns` and a `filter_column`.
    pub fn new(table: Table, columns: Vec<Column<F>>, filter_column: Option<Column<F>>) -> Self {
        Self {
            table,
            columns,
            filter_column,
        }
    }
}

/// Cross-table lookup data consisting in the lookup table (`looked_table`) and all the tables that look into `looked_table` (`looking_tables`).
/// Each `looking_table` corresponds to a STARK's table whose rows have been filtered out and whose columns have been through a linear combination (see `eval_table`). The concatenation of those smaller tables should result in the `looked_table`.
#[derive(Clone)]
pub struct CrossTableLookup<F: Field> {
    /// Column linear combinations for all tables that are looking into the current table.
    pub(crate) looking_tables: Vec<TableWithColumns<F>>,
    /// Column linear combination for the current table.
    pub(crate) looked_table: TableWithColumns<F>,
}

impl<F: Field> CrossTableLookup<F> {
    /// Creates a new `CrossTableLookup` given some looking tables and a looked table.
    /// All tables should have the same width.
    pub fn new(
        looking_tables: Vec<TableWithColumns<F>>,
        looked_table: TableWithColumns<F>,
    ) -> Self {
        assert!(looking_tables
            .iter()
            .all(|twc| twc.columns.len() == looked_table.columns.len()));
        Self {
            looking_tables,
            looked_table,
        }
    }

    /// Given a `Table` t and the number of challenges, returns the number of Cross-table lookup polynomials associated to t,
    /// i.e. the number of looking and looked tables among all CTLs whose columns are taken from t.
    pub(crate) fn num_ctl_zs(ctls: &[Self], table: Table, num_challenges: usize) -> usize {
        let mut num_ctls = 0;
        for ctl in ctls {
            let all_tables = std::iter::once(&ctl.looked_table).chain(&ctl.looking_tables);
            num_ctls += all_tables.filter(|twc| twc.table == table).count();
        }
        num_ctls * num_challenges
    }
}

/// Cross-table lookup data for one table.
#[derive(Clone, Default)]
pub struct CtlData<F: Field> {
    /// Data associated with all Z(x) polynomials for one table.
    pub(crate) zs_columns: Vec<CtlZData<F>>,
}

/// Cross-table lookup data associated with one Z(x) polynomial.
#[derive(Clone)]
pub(crate) struct CtlZData<F: Field> {
    /// Z polynomial values.
    pub(crate) z: PolynomialValues<F>,
    /// Cross-table lookup challenge.
    pub(crate) challenge: GrandProductChallenge<F>,
    /// Column linear combination for the current table.
    pub(crate) columns: Vec<Column<F>>,
    /// Filter column for the current table. It evaluates to either 1 or 0.
    pub(crate) filter_column: Option<Column<F>>,
}

impl<F: Field> CtlData<F> {
    /// Returns the number of cross-table lookup polynomials.
    pub fn len(&self) -> usize {
        self.zs_columns.len()
    }

    /// Returns whether there are no cross-table lookups.
    pub fn is_empty(&self) -> bool {
        self.zs_columns.is_empty()
    }

    /// Returns all the cross-table lookup polynomials.
    pub fn z_polys(&self) -> Vec<PolynomialValues<F>> {
        self.zs_columns
            .iter()
            .map(|zs_columns| zs_columns.z.clone())
            .collect()
    }
}

/// Randomness for a single instance of a permutation check protocol.
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub(crate) struct GrandProductChallenge<T: Copy + Eq + PartialEq + Debug> {
    /// Randomness used to combine multiple columns into one.
    pub(crate) beta: T,
    /// Random offset that's added to the beta-reduced column values.
    pub(crate) gamma: T,
}

impl<F: Field> GrandProductChallenge<F> {
    pub(crate) fn combine<'a, FE, P, T: IntoIterator<Item = &'a P>, const D2: usize>(
        &self,
        terms: T,
    ) -> P
    where
        FE: FieldExtension<D2, BaseField = F>,
        P: PackedField<Scalar = FE>,
        T::IntoIter: DoubleEndedIterator,
    {
        reduce_with_powers(terms, FE::from_basefield(self.beta)) + FE::from_basefield(self.gamma)
    }
}

impl GrandProductChallenge<Target> {
    pub(crate) fn combine_circuit<F: RichField + Extendable<D>, const D: usize>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        terms: &[ExtensionTarget<D>],
    ) -> ExtensionTarget<D> {
        let reduced = reduce_with_powers_ext_circuit(builder, terms, self.beta);
        let gamma = builder.convert_to_ext(self.gamma);
        builder.add_extension(reduced, gamma)
    }
}

impl GrandProductChallenge<Target> {
    pub(crate) fn combine_base_circuit<F: RichField + Extendable<D>, const D: usize>(
        &self,
        builder: &mut CircuitBuilder<F, D>,
        terms: &[Target],
    ) -> Target {
        let reduced = reduce_with_powers_circuit(builder, terms, self.beta);
        builder.add(reduced, self.gamma)
    }
}

/// Like `PermutationChallenge`, but with `num_challenges` copies to boost soundness.
#[derive(Clone, Eq, PartialEq, Debug)]
pub(crate) struct GrandProductChallengeSet<T: Copy + Eq + PartialEq + Debug> {
    pub(crate) challenges: Vec<GrandProductChallenge<T>>,
}

impl GrandProductChallengeSet<Target> {
    pub fn to_buffer(&self, buffer: &mut Vec<u8>) -> IoResult<()> {
        buffer.write_usize(self.challenges.len())?;
        for challenge in &self.challenges {
            buffer.write_target(challenge.beta)?;
            buffer.write_target(challenge.gamma)?;
        }
        Ok(())
    }

    pub fn from_buffer(buffer: &mut Buffer) -> IoResult<Self> {
        let length = buffer.read_usize()?;
        let mut challenges = Vec::with_capacity(length);
        for _ in 0..length {
            challenges.push(GrandProductChallenge {
                beta: buffer.read_target()?,
                gamma: buffer.read_target()?,
            });
        }

        Ok(GrandProductChallengeSet { challenges })
    }
}

fn get_grand_product_challenge<F: RichField, H: Hasher<F>>(
    challenger: &mut Challenger<F, H>,
) -> GrandProductChallenge<F> {
    let beta = challenger.get_challenge();
    let gamma = challenger.get_challenge();
    GrandProductChallenge { beta, gamma }
}

pub(crate) fn get_grand_product_challenge_set<F: RichField, H: Hasher<F>>(
    challenger: &mut Challenger<F, H>,
    num_challenges: usize,
) -> GrandProductChallengeSet<F> {
    let challenges = (0..num_challenges)
        .map(|_| get_grand_product_challenge(challenger))
        .collect();
    GrandProductChallengeSet { challenges }
}

fn get_grand_product_challenge_target<
    F: RichField + Extendable<D>,
    H: AlgebraicHasher<F>,
    const D: usize,
>(
    builder: &mut CircuitBuilder<F, D>,
    challenger: &mut RecursiveChallenger<F, H, D>,
) -> GrandProductChallenge<Target> {
    let beta = challenger.get_challenge(builder);
    let gamma = challenger.get_challenge(builder);
    GrandProductChallenge { beta, gamma }
}

pub(crate) fn get_grand_product_challenge_set_target<
    F: RichField + Extendable<D>,
    H: AlgebraicHasher<F>,
    const D: usize,
>(
    builder: &mut CircuitBuilder<F, D>,
    challenger: &mut RecursiveChallenger<F, H, D>,
    num_challenges: usize,
) -> GrandProductChallengeSet<Target> {
    let challenges = (0..num_challenges)
        .map(|_| get_grand_product_challenge_target(builder, challenger))
        .collect();
    GrandProductChallengeSet { challenges }
}

/// Generates all the cross-table lookup data, for all tables.
/// - `trace_poly_values` corresponds to the trace values for all tables.
/// - `cross_table_lookups` corresponds to all the cross-table lookups, i.e. the looked and looking tables, as described in `CrossTableLookup`.
/// - `ctl_challenges` corresponds to the challenges used for CTLs.
/// For each `CrossTableLookup`, and each looking/looked table, the partial products for the CTL are computed, and added to the said table's `CtlZData`.
pub(crate) fn cross_table_lookup_data<F: RichField, const D: usize>(
    trace_poly_values: &[Vec<PolynomialValues<F>>; NUM_TABLES],
    cross_table_lookups: &[CrossTableLookup<F>],
    ctl_challenges: &GrandProductChallengeSet<F>,
) -> [CtlData<F>; NUM_TABLES] {
    let mut ctl_data_per_table = [0; NUM_TABLES].map(|_| CtlData::default());
    for CrossTableLookup {
        looking_tables,
        looked_table,
    } in cross_table_lookups
    {
        log::debug!("Processing CTL for {:?}", looked_table.table);
        for &challenge in &ctl_challenges.challenges {
            let zs_looking = looking_tables.iter().map(|table| {
                partial_products(
                    &trace_poly_values[table.table as usize],
                    &table.columns,
                    &table.filter_column,
                    challenge,
                )
            });
            let z_looked = partial_products(
                &trace_poly_values[looked_table.table as usize],
                &looked_table.columns,
                &looked_table.filter_column,
                challenge,
            );
            for (table, z) in looking_tables.iter().zip(zs_looking) {
                ctl_data_per_table[table.table as usize]
                    .zs_columns
                    .push(CtlZData {
                        z,
                        challenge,
                        columns: table.columns.clone(),
                        filter_column: table.filter_column.clone(),
                    });
            }
            ctl_data_per_table[looked_table.table as usize]
                .zs_columns
                .push(CtlZData {
                    z: z_looked,
                    challenge,
                    columns: looked_table.columns.clone(),
                    filter_column: looked_table.filter_column.clone(),
                });
        }
    }
    ctl_data_per_table
}

/// Computes the cross-table lookup partial products for one table and given column linear combinations.
/// `trace` represents the trace values for the given table.
/// `columns` are all the column linear combinations to evaluate.
/// `filter_column` is a column linear combination used to determine whether a row should be selected.
/// `challenge` is a cross-table lookup challenge.
/// The initial product `p` is 1.
/// For each row, if the `filter_column` evaluates to 1, then the rows is selected. All the column linear combinations are evaluated at said row. All those evaluations are combined using the challenge to get a value `v`.
/// The product is updated: `p *= v`, and is pushed to the vector of partial products.
fn partial_products<F: Field>(
    trace: &[PolynomialValues<F>],
    columns: &[Column<F>],
    filter_column: &Option<Column<F>>,
    challenge: GrandProductChallenge<F>,
) -> PolynomialValues<F> {
    let mut partial_prod = F::ONE;
    let degree = trace[0].len();
    let mut res = Vec::with_capacity(degree);
    for i in (0..degree).rev() {
        let filter = if let Some(column) = filter_column {
            column.eval_table(trace, i)
        } else {
            F::ONE
        };
        if filter.is_one() {
            let evals = columns
                .iter()
                .map(|c| c.eval_table(trace, i))
                .collect::<Vec<_>>();
            partial_prod *= challenge.combine(evals.iter());
        } else {
            assert_eq!(filter, F::ZERO, "Non-binary filter?")
        };
        res.push(partial_prod);
    }
    res.reverse();
    res.into()
}

/// Data necessary to check the cross-table lookups of a given table.
#[derive(Clone)]
pub struct CtlCheckVars<'a, F, FE, P, const D2: usize>
where
    F: Field,
    FE: FieldExtension<D2, BaseField = F>,
    P: PackedField<Scalar = FE>,
{
    /// Evaluation of the trace polynomials at point `zeta`.
    pub(crate) local_z: P,
    /// Evaluation of the trace polynomials at point `g * zeta`
    pub(crate) next_z: P,
    /// Cross-table lookup challenges.
    pub(crate) challenges: GrandProductChallenge<F>,
    /// Column linear combinations of the `CrossTableLookup`s.
    pub(crate) columns: &'a [Column<F>],
    /// Column linear combination that evaluates to either 1 or 0.
    pub(crate) filter_column: &'a Option<Column<F>>,
}

impl<'a, F: RichField + Extendable<D>, const D: usize>
    CtlCheckVars<'a, F, F::Extension, F::Extension, D>
{
    /// Extracts the `CtlCheckVars` for each STARK.
    pub(crate) fn from_proofs<C: GenericConfig<D, F = F>>(
        proofs: &[StarkProofWithMetadata<F, C, D>; NUM_TABLES],
        cross_table_lookups: &'a [CrossTableLookup<F>],
        ctl_challenges: &'a GrandProductChallengeSet<F>,
        num_lookup_columns: &[usize; NUM_TABLES],
    ) -> [Vec<Self>; NUM_TABLES] {
        // Get all cross-table lookup polynomial openings for each STARK proof.
        let mut ctl_zs = proofs
            .iter()
            .zip(num_lookup_columns)
            .map(|(p, &num_lookup)| {
                let openings = &p.proof.openings;
                let ctl_zs = openings.auxiliary_polys.iter().skip(num_lookup);
                let ctl_zs_next = openings.auxiliary_polys_next.iter().skip(num_lookup);
                ctl_zs.zip(ctl_zs_next)
            })
            .collect::<Vec<_>>();

        // Put each cross-table lookup polynomial into the correct table data: if a CTL polynomial is extracted from looking/looked table t, then we add it to the `CtlCheckVars` of table t.
        let mut ctl_vars_per_table = [0; NUM_TABLES].map(|_| vec![]);
        for CrossTableLookup {
            looking_tables,
            looked_table,
        } in cross_table_lookups
        {
            for &challenges in &ctl_challenges.challenges {
                for table in looking_tables {
                    let (looking_z, looking_z_next) = ctl_zs[table.table as usize].next().unwrap();
                    ctl_vars_per_table[table.table as usize].push(Self {
                        local_z: *looking_z,
                        next_z: *looking_z_next,
                        challenges,
                        columns: &table.columns,
                        filter_column: &table.filter_column,
                    });
                }

                let (looked_z, looked_z_next) = ctl_zs[looked_table.table as usize].next().unwrap();
                ctl_vars_per_table[looked_table.table as usize].push(Self {
                    local_z: *looked_z,
                    next_z: *looked_z_next,
                    challenges,
                    columns: &looked_table.columns,
                    filter_column: &looked_table.filter_column,
                });
            }
        }
        ctl_vars_per_table
    }
}

/// Checks the cross-table lookup Z polynomials for each table:
/// - Checks that the CTL `Z` partial products are correctly updated.
/// - Checks that the final value of the CTL product is the combination of all STARKs' CTL polynomials.
/// CTL `Z` partial products are upside down: the complete product is on the first row, and
/// the first term is on the last row. This allows the transition constraint to be:
/// Z(w) = Z(gw) * combine(w) where combine is called on the local row
/// and not the next. This enables CTLs across two rows.
pub(crate) fn eval_cross_table_lookup_checks<F, FE, P, S, const D: usize, const D2: usize>(
    vars: &S::EvaluationFrame<FE, P, D2>,
    ctl_vars: &[CtlCheckVars<F, FE, P, D2>],
    consumer: &mut ConstraintConsumer<P>,
) where
    F: RichField + Extendable<D>,
    FE: FieldExtension<D2, BaseField = F>,
    P: PackedField<Scalar = FE>,
    S: Stark<F, D>,
{
    let local_values = vars.get_local_values();
    let next_values = vars.get_next_values();

    for lookup_vars in ctl_vars {
        let CtlCheckVars {
            local_z,
            next_z,
            challenges,
            columns,
            filter_column,
        } = lookup_vars;

        // Compute all linear combinations on the current table, and combine them using the challenge.
        let evals = columns
            .iter()
            .map(|c| c.eval_with_next(local_values, next_values))
            .collect::<Vec<_>>();
        let combined = challenges.combine(evals.iter());
        let local_filter = if let Some(column) = filter_column {
            column.eval_with_next(local_values, next_values)
        } else {
            P::ONES
        };
        // If the filter evaluates to 1, then the previously computed combination is used.
        let select = local_filter * combined + P::ONES - local_filter;

        // Check value of `Z(g^(n-1))`
        consumer.constraint_last_row(*local_z - select);
        // Check `Z(w) = combination * Z(gw)`
        consumer.constraint_transition(*next_z * select - *local_z);
    }
}

/// Circuit version of `CtlCheckVars`. Data necessary to check the cross-table lookups of a given table.
#[derive(Clone)]
pub struct CtlCheckVarsTarget<'a, F: Field, const D: usize> {
    /// Evaluation of the trace polynomials at point `zeta`.
    pub(crate) local_z: ExtensionTarget<D>,
    /// Evaluation of the trace polynomials at point `g * zeta`.
    pub(crate) next_z: ExtensionTarget<D>,
    /// Cross-table lookup challenges.
    pub(crate) challenges: GrandProductChallenge<Target>,
    /// Column linear combinations of the `CrossTableLookup`s.
    pub(crate) columns: &'a [Column<F>],
    /// Column linear combination that evaluates to either 1 or 0.
    pub(crate) filter_column: &'a Option<Column<F>>,
}

impl<'a, F: Field, const D: usize> CtlCheckVarsTarget<'a, F, D> {
    /// Circuit version of `from_proofs`. Extracts the `CtlCheckVarsTarget` for each STARK.
    pub(crate) fn from_proof(
        table: Table,
        proof: &StarkProofTarget<D>,
        cross_table_lookups: &'a [CrossTableLookup<F>],
        ctl_challenges: &'a GrandProductChallengeSet<Target>,
        num_lookup_columns: usize,
    ) -> Vec<Self> {
        // Get all cross-table lookup polynomial openings for each STARK proof.
        let mut ctl_zs = {
            let openings = &proof.openings;
            let ctl_zs = openings.auxiliary_polys.iter().skip(num_lookup_columns);
            let ctl_zs_next = openings
                .auxiliary_polys_next
                .iter()
                .skip(num_lookup_columns);
            ctl_zs.zip(ctl_zs_next)
        };

        // Put each cross-table lookup polynomial into the correct table data: if a CTL polynomial is extracted from looking/looked table t, then we add it to the `CtlCheckVars` of table t.
        let mut ctl_vars = vec![];
        for CrossTableLookup {
            looking_tables,
            looked_table,
        } in cross_table_lookups
        {
            for &challenges in &ctl_challenges.challenges {
                for looking_table in looking_tables {
                    if looking_table.table == table {
                        let (looking_z, looking_z_next) = ctl_zs.next().unwrap();
                        ctl_vars.push(Self {
                            local_z: *looking_z,
                            next_z: *looking_z_next,
                            challenges,
                            columns: &looking_table.columns,
                            filter_column: &looking_table.filter_column,
                        });
                    }
                }

                if looked_table.table == table {
                    let (looked_z, looked_z_next) = ctl_zs.next().unwrap();
                    ctl_vars.push(Self {
                        local_z: *looked_z,
                        next_z: *looked_z_next,
                        challenges,
                        columns: &looked_table.columns,
                        filter_column: &looked_table.filter_column,
                    });
                }
            }
        }
        assert!(ctl_zs.next().is_none());
        ctl_vars
    }
}

/// Circuit version of `eval_cross_table_lookup_checks`. Checks the cross-table lookups for each table:
/// - Checks that the CTL `Z` partial products are correctly updated.
/// - Checks that the final value of the CTL product is the combination of all STARKs' CTL polynomials.
/// CTL `Z` partial products are upside down: the complete product is on the first row, and
/// the first term is on the last row. This allows the transition constraint to be:
/// Z(w) = Z(gw) * combine(w) where combine is called on the local row
/// and not the next. This enables CTLs across two rows.
pub(crate) fn eval_cross_table_lookup_checks_circuit<
    S: Stark<F, D>,
    F: RichField + Extendable<D>,
    const D: usize,
>(
    builder: &mut CircuitBuilder<F, D>,
    vars: &S::EvaluationFrameTarget,
    ctl_vars: &[CtlCheckVarsTarget<F, D>],
    consumer: &mut RecursiveConstraintConsumer<F, D>,
) {
    let local_values = vars.get_local_values();
    let next_values = vars.get_next_values();

    for lookup_vars in ctl_vars {
        let CtlCheckVarsTarget {
            local_z,
            next_z,
            challenges,
            columns,
            filter_column,
        } = lookup_vars;

        let one = builder.one_extension();
        let local_filter = if let Some(column) = filter_column {
            column.eval_circuit(builder, local_values)
        } else {
            one
        };
        fn select<F: RichField + Extendable<D>, const D: usize>(
            builder: &mut CircuitBuilder<F, D>,
            filter: ExtensionTarget<D>,
            x: ExtensionTarget<D>,
        ) -> ExtensionTarget<D> {
            let one = builder.one_extension();
            let tmp = builder.sub_extension(one, filter);
            builder.mul_add_extension(filter, x, tmp) // filter * x + 1 - filter
        }

        // Compute all linear combinations on the current table, and combine them using the challenge.
        let evals = columns
            .iter()
            .map(|c| c.eval_with_next_circuit(builder, local_values, next_values))
            .collect::<Vec<_>>();

        let combined = challenges.combine_circuit(builder, &evals);
        // If the filter evaluates to 1, then the previously computed combination is used.
        let select = select(builder, local_filter, combined);

        // Check value of `Z(g^(n-1))`
        let last_row = builder.sub_extension(*local_z, select);
        consumer.constraint_last_row(builder, last_row);
        // Check `Z(w) = combination * Z(gw)`
        let transition = builder.mul_sub_extension(*next_z, select, *local_z);
        consumer.constraint_transition(builder, transition);
    }
}

/// Verifies all cross-table lookups.
pub(crate) fn verify_cross_table_lookups<F: RichField + Extendable<D>, const D: usize>(
    cross_table_lookups: &[CrossTableLookup<F>],
    ctl_zs_first: [Vec<F>; NUM_TABLES],
    ctl_extra_looking_products: Vec<Vec<F>>,
    config: &StarkConfig,
) -> Result<()> {
    let mut ctl_zs_openings = ctl_zs_first.iter().map(|v| v.iter()).collect::<Vec<_>>();
    for (
        index,
        CrossTableLookup {
            looking_tables,
            looked_table,
        },
    ) in cross_table_lookups.iter().enumerate()
    {
        // Get elements looking into `looked_table` that are not associated to any STARK.
        let extra_product_vec = &ctl_extra_looking_products[looked_table.table as usize];
        for c in 0..config.num_challenges {
            // Compute the combination of all looking table CTL polynomial openings.
            let looking_zs_prod = looking_tables
                .iter()
                .map(|table| *ctl_zs_openings[table.table as usize].next().unwrap())
                .product::<F>()
                * extra_product_vec[c];

            // Get the looked table CTL polynomial opening.
            let looked_z = *ctl_zs_openings[looked_table.table as usize].next().unwrap();
            // Ensure that the combination of looking table openings is equal to the looked table opening.
            ensure!(
                looking_zs_prod == looked_z,
                "Cross-table lookup {:?} verification failed.",
                index
            );
        }
    }
    debug_assert!(ctl_zs_openings.iter_mut().all(|iter| iter.next().is_none()));

    Ok(())
}

/// Circuit version of `verify_cross_table_lookups`. Verifies all cross-table lookups.
pub(crate) fn verify_cross_table_lookups_circuit<F: RichField + Extendable<D>, const D: usize>(
    builder: &mut CircuitBuilder<F, D>,
    cross_table_lookups: Vec<CrossTableLookup<F>>,
    ctl_zs_first: [Vec<Target>; NUM_TABLES],
    ctl_extra_looking_products: Vec<Vec<Target>>,
    inner_config: &StarkConfig,
) {
    let mut ctl_zs_openings = ctl_zs_first.iter().map(|v| v.iter()).collect::<Vec<_>>();
    for CrossTableLookup {
        looking_tables,
        looked_table,
    } in cross_table_lookups.into_iter()
    {
        // Get elements looking into `looked_table` that are not associated to any STARK.
        let extra_product_vec = &ctl_extra_looking_products[looked_table.table as usize];
        for c in 0..inner_config.num_challenges {
            // Compute the combination of all looking table CTL polynomial openings.
            let mut looking_zs_prod = builder.mul_many(
                looking_tables
                    .iter()
                    .map(|table| *ctl_zs_openings[table.table as usize].next().unwrap()),
            );

            looking_zs_prod = builder.mul(looking_zs_prod, extra_product_vec[c]);

            // Get the looked table CTL polynomial opening.
            let looked_z = *ctl_zs_openings[looked_table.table as usize].next().unwrap();
            // Verify that the combination of looking table openings is equal to the looked table opening.
            builder.connect(looked_z, looking_zs_prod);
        }
    }
    debug_assert!(ctl_zs_openings.iter_mut().all(|iter| iter.next().is_none()));
}

#[cfg(test)]
pub(crate) mod testutils {
    use std::collections::HashMap;

    use plonky2::field::polynomial::PolynomialValues;
    use plonky2::field::types::Field;

    use crate::all_stark::Table;
    use crate::cross_table_lookup::{CrossTableLookup, TableWithColumns};

    type MultiSet<F> = HashMap<Vec<F>, Vec<(Table, usize)>>;

    /// Check that the provided traces and cross-table lookups are consistent.
    pub(crate) fn check_ctls<F: Field>(
        trace_poly_values: &[Vec<PolynomialValues<F>>],
        cross_table_lookups: &[CrossTableLookup<F>],
        extra_memory_looking_values: &[Vec<F>],
    ) {
        for (i, ctl) in cross_table_lookups.iter().enumerate() {
            check_ctl(trace_poly_values, ctl, i, extra_memory_looking_values);
        }
    }

    fn check_ctl<F: Field>(
        trace_poly_values: &[Vec<PolynomialValues<F>>],
        ctl: &CrossTableLookup<F>,
        ctl_index: usize,
        extra_memory_looking_values: &[Vec<F>],
    ) {
        let CrossTableLookup {
            looking_tables,
            looked_table,
        } = ctl;

        // Maps `m` with `(table, i) in m[row]` iff the `i`-th row of `table` is equal to `row` and
        // the filter is 1. Without default values, the CTL check holds iff `looking_multiset == looked_multiset`.
        let mut looking_multiset = MultiSet::<F>::new();
        let mut looked_multiset = MultiSet::<F>::new();

        for table in looking_tables {
            process_table(trace_poly_values, table, &mut looking_multiset);
        }
        process_table(trace_poly_values, looked_table, &mut looked_multiset);

        // Extra looking values for memory
        if ctl_index == Table::Memory as usize {
            for row in extra_memory_looking_values.iter() {
                // The table and the row index don't matter here, as we just want to enforce
                // that the special extra values do appear when looking against the Memory table.
                looking_multiset
                    .entry(row.to_vec())
                    .or_default()
                    .push((Table::Cpu, 0));
            }
        }

        let empty = &vec![];
        // Check that every row in the looking tables appears in the looked table the same number of times.
        for (row, looking_locations) in &looking_multiset {
            let looked_locations = looked_multiset.get(row).unwrap_or(empty);
            check_locations(looking_locations, looked_locations, ctl_index, row);
        }
        // Check that every row in the looked tables appears in the looked table the same number of times.
        for (row, looked_locations) in &looked_multiset {
            let looking_locations = looking_multiset.get(row).unwrap_or(empty);
            check_locations(looking_locations, looked_locations, ctl_index, row);
        }
    }

    fn process_table<F: Field>(
        trace_poly_values: &[Vec<PolynomialValues<F>>],
        table: &TableWithColumns<F>,
        multiset: &mut MultiSet<F>,
    ) {
        let trace = &trace_poly_values[table.table as usize];
        for i in 0..trace[0].len() {
            let filter = if let Some(column) = &table.filter_column {
                column.eval_table(trace, i)
            } else {
                F::ONE
            };
            if filter.is_one() {
                let row = table
                    .columns
                    .iter()
                    .map(|c| c.eval_table(trace, i))
                    .collect::<Vec<_>>();
                multiset.entry(row).or_default().push((table.table, i));
            } else {
                assert_eq!(filter, F::ZERO, "Non-binary filter?")
            }
        }
    }

    fn check_locations<F: Field>(
        looking_locations: &[(Table, usize)],
        looked_locations: &[(Table, usize)],
        ctl_index: usize,
        row: &[F],
    ) {
        if looking_locations.len() != looked_locations.len() {
            panic!(
                "CTL #{ctl_index}:\n\
                 Row {row:?} is present {l0} times in the looking tables, but {l1} times in the looked table.\n\
                 Looking locations (Table, Row index): {looking_locations:?}.\n\
                 Looked locations (Table, Row index): {looked_locations:?}.",
                l0 = looking_locations.len(),
                l1 = looked_locations.len(),
            );
        }
    }
}
