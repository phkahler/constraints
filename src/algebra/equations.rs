use crate::algebra::{
    ops::{self, Context, EvaluationError},
    Expression, Parameter, ParseError,
};
use nalgebra::{DMatrix as Matrix, DVector as Vector};
use std::{
    collections::HashMap,
    fmt::Debug,
    iter::{Extend, FromIterator},
    str::FromStr,
};

#[derive(Debug, Clone, PartialEq)]
pub struct Equation {
    body: Expression,
}

impl Equation {
    pub fn new(left: Expression, right: Expression) -> Self {
        debug_assert_ne!(
            left.params().count() + right.params().count(),
            0,
            "Equations should contain at least one unknown"
        );
        Equation { body: left - right }
    }
}

impl FromStr for Equation {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.find("=") {
            Some(index) => {
                let (left, right) = s.split_at(index);
                let right = &right[1..];
                Ok(Equation::new(left.parse()?, right.parse()?))
            },
            None => Ok(Equation { body: s.parse()? }),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SolveError {
    Eval(EvaluationError),
    DidntConverge,
}

impl From<EvaluationError> for SolveError {
    fn from(e: EvaluationError) -> Self { SolveError::Eval(e) }
}

/// A builder for constructing a system of equations and solving them.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct SystemOfEquations {
    equations: Vec<Equation>,
}

impl SystemOfEquations {
    pub fn new() -> Self { SystemOfEquations::default() }

    pub fn with(mut self, equation: Equation) -> Self {
        self.push(equation);
        self
    }

    pub fn push(&mut self, equation: Equation) {
        self.equations.push(equation);
    }

    pub fn solve<C>(self, ctx: &C) -> Result<Solution, SolveError>
    where
        C: Context,
    {
        let unknowns = self.unknowns();
        let jacobian =
            Jacobian::for_equations(&self.equations, &unknowns, ctx)?;
        let got = solve_with_newtons_method(&jacobian, &self, ctx)?;

        Ok(Solution {
            known_values: jacobian.collate_unknowns(got.as_slice()),
        })
    }

    pub fn unknowns(&self) -> Vec<Parameter> {
        let mut unknowns: Vec<_> = self
            .equations
            .iter()
            .flat_map(|eq| eq.body.params())
            .cloned()
            .collect();
        unknowns.sort();
        unknowns.dedup();

        unknowns
    }

    pub fn num_unknowns(&self) -> usize { self.unknowns().len() }

    pub fn from_equations<E, S>(equations: E) -> Result<Self, ParseError>
    where
        E: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut system = SystemOfEquations::new();

        for equation in equations {
            system.push(equation.as_ref().parse()?);
        }

        Ok(system)
    }

    fn evaluate<F, C>(
        &self,
        lookup_parameter_value: F,
        ctx: &C,
    ) -> Result<Vector<f64>, EvaluationError>
    where
        F: Fn(&Parameter) -> Option<f64>,
        C: Context,
    {
        let mut values = Vec::new();

        for equation in &self.equations {
            values.push(ops::evaluate(
                &equation.body,
                &lookup_parameter_value,
                ctx,
            )?);
        }

        Ok(Vector::from_vec(values))
    }
}

impl Extend<Equation> for SystemOfEquations {
    fn extend<T: IntoIterator<Item = Equation>>(&mut self, iter: T) {
        self.equations.extend(iter);
    }
}

impl FromIterator<Equation> for SystemOfEquations {
    fn from_iter<T: IntoIterator<Item = Equation>>(iter: T) -> Self {
        SystemOfEquations {
            equations: Vec::from_iter(iter),
        }
    }
}

/// Solve a set of non-linear equations iteratively using Newton's method.
///
/// The iterative equation for Newton's method when applied to a set of
/// equations, `F`, is:
///
/// ```text
///  x_next = x_current - jacobian(F).inverse() * F(x_current)
/// ```
///
/// This is the multi-variable equivalent of Newton-Raphson, where the jacobian
/// is the slope of our equations, and we pre-multiply by the inverse because
/// that's the matrix equivalent of division.
///
/// Calculating the inverse of a matrix is expensive though, so we rearrange
/// it to look like this:
///
/// ```text
/// jacobian(F) * (x_next - x_current) = -F(x_current)
/// ```
///
/// ... Which is in the form `A.δx = b`.
///
/// We can then solve for `δx` using gaussian elimination, then get the refined
/// solution by solving `δx = x_next - x_current`.
///
/// See also:
///
/// - https://en.wikipedia.org/wiki/Newton%27s_method#Nonlinear_systems_of_equations
/// - https://www.youtube.com/watch?v=zPDp_ewoyhM
fn solve_with_newtons_method<C>(
    jacobian: &Jacobian,
    system: &SystemOfEquations,
    ctx: &C,
) -> Result<Vector<f64>, SolveError>
where
    C: Context,
{
    const MAX_ITERATIONS: usize = 50;

    let mut solution = jacobian.initial_values();

    for _ in 0..MAX_ITERATIONS {
        let x_next = {
            let evaluated_jacobian =
                jacobian.evaluate(solution.as_slice(), ctx)?;

            let lookup = jacobian.lookup_value_by_name(solution.as_slice());
            let f_of_x = system.evaluate(&lookup, ctx)?;
            step_newtons_method(evaluated_jacobian, &solution, f_of_x)
        };

        if approx::relative_eq!(x_next, solution) {
            return Ok(x_next);
        }
        solution = x_next;
    }

    Err(SolveError::DidntConverge)
}

fn step_newtons_method(
    jacobian: Matrix<f64>,
    x: &Vector<f64>,
    f_of_x: Vector<f64>,
) -> Vector<f64> {
    // We're trying to solve:
    //   x_next = x_current - jacobian(F).inverse() * F(x_current)
    //
    // Which gets rearranged as:
    //   jacobian(F) * (x_next - x_current) = -F(x_current)
    //
    // Note that we use LU decomposition to solve equations of the form `Ax = b`

    let negative_f_of_x = -f_of_x;
    let delta_x = jacobian.lu().solve(&negative_f_of_x).unwrap();

    delta_x + x
}

#[derive(Debug, Clone, PartialEq)]
pub struct Solution {
    known_values: HashMap<Parameter, f64>,
}

/// A matrix of [`Expression`]s representing the partial derivatives for each
/// parameter in each equation.
#[derive(Debug, Clone, PartialEq)]
struct Jacobian<'a> {
    cells: Box<[Expression]>,
    equations: &'a [Equation],
    unknowns: &'a [Parameter],
}

impl<'a> Jacobian<'a> {
    fn for_equations<C>(
        equations: &'a [Equation],
        unknowns: &'a [Parameter],
        ctx: &C,
    ) -> Result<Self, EvaluationError>
    where
        C: Context,
    {
        let mut cells = Vec::new();

        for equation in equations {
            for unknown in unknowns {
                let value = if equation.body.depends_on(unknown) {
                    let derivative =
                        ops::partial_derivative(&equation.body, unknown, ctx)?;
                    ops::fold_constants(&derivative, ctx)
                } else {
                    Expression::Constant(0.0)
                };
                cells.push(value);
            }
        }

        Ok(Jacobian {
            cells: cells.into_boxed_slice(),
            equations,
            unknowns,
        })
    }

    fn rows(&self) -> usize { self.equations.len() }

    fn columns(&self) -> usize { self.unknowns.len() }

    fn evaluate<C>(
        &self,
        parameter_values: &[f64],
        ctx: &C,
    ) -> Result<Matrix<f64>, EvaluationError>
    where
        C: Context,
    {
        assert_eq!(parameter_values.len(), self.unknowns.len());

        let mut values = Vec::with_capacity(self.cells.len());
        let lookup = self.lookup_value_by_name(parameter_values);

        for row in self.iter_rows() {
            for expression in row {
                values.push(ops::evaluate(&expression, &lookup, ctx)?);
            }
        }

        Ok(Matrix::from_vec(self.rows(), self.columns(), values))
    }

    fn lookup_value_by_name<'p>(
        &'p self,
        parameter_values: &'p [f64],
    ) -> impl Fn(&Parameter) -> Option<f64> + 'p {
        move |parameter| {
            self.unknowns
                .iter()
                .position(|p| p == parameter)
                .map(|ix| parameter_values[ix])
        }
    }

    fn collate_unknowns(
        &self,
        parameter_values: &[f64],
    ) -> HashMap<Parameter, f64> {
        self.unknowns
            .iter()
            .cloned()
            .zip(parameter_values.iter().copied())
            .collect()
    }

    fn initial_values(&self) -> Vector<f64> {
        Vector::zeros(self.unknowns.len())
    }

    fn iter_rows(&self) -> impl Iterator<Item = &[Expression]> + '_ {
        self.cells.chunks_exact(self.columns())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::algebra::ops::Builtins;

    #[test]
    fn single_equality() {
        let equation: Equation = "x = 5".parse().unwrap();
        let x = Parameter::named("x");
        let builtins = Builtins::default();

        let got = SystemOfEquations::new()
            .with(equation)
            .solve(&builtins)
            .unwrap();

        assert_eq!(got.known_values.len(), 1);
        assert_eq!(got.known_values[&x], 5.0);
    }

    #[test]
    fn calculate_jacobian_of_known_system_of_equations() {
        // See https://en.wikipedia.org/wiki/Jacobian_matrix_and_determinant#Example_5
        let system = SystemOfEquations::from_equations(&[
            "5 * b",
            "4*a*a - 2*sin(b*c)",
            "b*c",
        ])
        .unwrap();
        let ctx = Builtins::default();

        let unknowns = system.unknowns();
        let got = Jacobian::for_equations(&system.equations, &unknowns, &ctx)
            .unwrap();

        assert_eq!(
            got.columns(),
            system.num_unknowns(),
            "There are 3 unknowns"
        );
        assert_eq!(got.rows(), system.equations.len(), "There are 3 equations");
        // Note: I needed to rearrange the Wikipedia equations a bit because
        // our solver multiplied things differently (i.e. "c*2" instead of
        // "2*c")
        let should_be = [
            ["0", "5", "0"].as_ref(),
            ["8*a", "-cos(b*c)*c*2", "-cos(b*c)*b*2"].as_ref(),
            ["0", "c", "b"].as_ref(),
        ];
        assert_jacobian_eq(&got, should_be.as_ref());
    }

    fn assert_jacobian_eq(jacobian: &Jacobian, should_be: &[&[&str]]) {
        assert_eq!(jacobian.rows(), should_be.len());

        for (row, row_should_be) in jacobian.iter_rows().zip(should_be) {
            assert_eq!(row.len(), row_should_be.len());

            for (value, column_should_be) in row.iter().zip(*row_should_be) {
                let should_be: Expression = column_should_be.parse().unwrap();

                // Usually I wouldn't compare strings, but it's possible to get
                // different (but equivalent!) trees when calculating the
                // jacobian vs parsing from a string
                assert_eq!(value.to_string(), should_be.to_string());
            }
        }
    }

    #[test]
    fn solve_simple_equations() {
        let system =
            SystemOfEquations::from_equations(&["x-1", "y-2", "z-3"]).unwrap();
        let ctx = Builtins::default();
        let unknowns = system.unknowns();
        let jacobian =
            Jacobian::for_equations(&system.equations, &unknowns, &ctx)
                .unwrap();

        let got = solve_with_newtons_method(&jacobian, &system, &ctx).unwrap();

        let named_parameters = jacobian.collate_unknowns(got.as_slice());
        let x = Parameter::named("x");
        let y = Parameter::named("y");
        let z = Parameter::named("z");
        assert_eq!(named_parameters[&x], 1.0);
        assert_eq!(named_parameters[&y], 2.0);
        assert_eq!(named_parameters[&z], 3.0);
    }

    #[test]
    fn work_through_youtube_example() {
        // From https://www.youtube.com/watch?v=zPDp_ewoyhM
        let system = SystemOfEquations::from_equations(&[
            "a + 2*b - 2",
            "a*a + 4*b*b - 4",
        ])
        .unwrap();
        let ctx = Builtins::default();

        // first we need to calculate the jacobian
        let unknowns = system.unknowns();
        let jacobian =
            Jacobian::for_equations(&system.equations, &unknowns, &ctx)
                .unwrap();
        assert_jacobian_eq(
            &jacobian,
            &[&["1 + -0", "2"], &["2*a + -0", "8*b"]],
        );

        // make an initial guess
        let x_0 = Vector::from_vec(vec![1.0, 2.0]);

        // evaluate the components we need
        let jacobian_of_x_0 = jacobian.evaluate(x_0.as_slice(), &ctx).unwrap();
        let lookup_parameter_value =
            jacobian.lookup_value_by_name(x_0.as_slice());
        let f_of_x_0 = system.evaluate(lookup_parameter_value, &ctx).unwrap();

        // and double-check them
        assert_eq!(
            jacobian_of_x_0,
            Matrix::from_vec(2, 2, vec![1.0, 2.0, 2.0, 16.0])
        );
        assert_eq!(f_of_x_0.as_slice(), &[3.0, 13.0]);

        // one iteration of newton's method
        let x_1 = step_newtons_method(jacobian_of_x_0, &x_0, f_of_x_0);
        let should_be = Vector::from_vec(vec![-10.0 / 12.0, 17.0 / 12.0]);
        approx::relative_eq!(x_1, should_be);
    }
}
