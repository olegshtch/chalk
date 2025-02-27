use chalk_engine::fallible::*;
use chalk_ir::cast::Cast;
use chalk_ir::fold::{
    DefaultFreeVarFolder, DefaultTypeFolder, Fold, InferenceFolder, PlaceholderFolder,
};
use chalk_ir::zip::{Zip, Zipper};
use std::sync::Arc;

use super::var::*;
use super::*;

impl InferenceTable {
    pub(crate) fn unify<T>(
        &mut self,
        environment: &Arc<Environment>,
        a: &T,
        b: &T,
    ) -> Fallible<UnificationResult>
    where
        T: ?Sized + Zip,
    {
        debug_heading!(
            "unify(a={:?}\
             ,\n      b={:?})",
            a,
            b
        );
        let snapshot = self.snapshot();
        match Unifier::new(self, environment).unify(a, b) {
            Ok(r) => {
                self.commit(snapshot);
                Ok(r)
            }
            Err(e) => {
                self.rollback_to(snapshot);
                Err(e)
            }
        }
    }
}

struct Unifier<'t> {
    table: &'t mut InferenceTable,
    environment: &'t Arc<Environment>,
    goals: Vec<InEnvironment<DomainGoal>>,
    constraints: Vec<InEnvironment<Constraint>>,
}

#[derive(Debug)]
pub(crate) struct UnificationResult {
    pub(crate) goals: Vec<InEnvironment<DomainGoal>>,
    pub(crate) constraints: Vec<InEnvironment<Constraint>>,
}

impl<'t> Unifier<'t> {
    fn new(table: &'t mut InferenceTable, environment: &'t Arc<Environment>) -> Self {
        Unifier {
            environment: environment,
            table: table,
            goals: vec![],
            constraints: vec![],
        }
    }

    /// The main entry point for the `Unifier` type and really the
    /// only type meant to be called externally. Performs a
    /// unification of `a` and `b` and returns the Unification Result.
    fn unify<T>(mut self, a: &T, b: &T) -> Fallible<UnificationResult>
    where
        T: ?Sized + Zip,
    {
        Zip::zip_with(&mut self, a, b)?;
        Ok(UnificationResult {
            goals: self.goals,
            constraints: self.constraints,
        })
    }

    /// When we encounter a "sub-unification" problem that is in a distinct
    /// environment, we invoke this routine.
    fn sub_unify<T>(&mut self, ty1: T, ty2: T) -> Fallible<()>
    where
        T: Zip + Fold,
    {
        let sub_unifier = Unifier::new(self.table, &self.environment);
        let UnificationResult { goals, constraints } = sub_unifier.unify(&ty1, &ty2)?;
        self.goals.extend(goals);
        self.constraints.extend(constraints);
        Ok(())
    }

    fn unify_ty_ty<'a>(&mut self, a: &'a Ty, b: &'a Ty) -> Fallible<()> {
        //         ^^                 ^^         ^^ FIXME rustc bug
        if let Some(n_a) = self.table.normalize_shallow(a) {
            return self.unify_ty_ty(&n_a, b);
        } else if let Some(n_b) = self.table.normalize_shallow(b) {
            return self.unify_ty_ty(a, &n_b);
        }

        debug_heading!(
            "unify_ty_ty(a={:?}\
             ,\n            b={:?})",
            a,
            b
        );

        match (a, b) {
            // Unifying two inference variables: unify them in the underlying
            // ena table.
            (&Ty::InferenceVar(var1), &Ty::InferenceVar(var2)) => {
                debug!("unify_ty_ty: unify_var_var({:?}, {:?})", var1, var2);
                let var1 = EnaVariable::from(var1);
                let var2 = EnaVariable::from(var2);
                Ok(self
                    .table
                    .unify
                    .unify_var_var(var1, var2)
                    .expect("unification of two unbound variables cannot fail"))
            }

            // Unifying an inference variables with a non-inference variable.
            (&Ty::InferenceVar(var), ty @ &Ty::Apply(_))
            | (ty @ &Ty::Apply(_), &Ty::InferenceVar(var))
            | (&Ty::InferenceVar(var), ty @ &Ty::Opaque(_))
            | (ty @ Ty::Opaque(_), &Ty::InferenceVar(var))
            | (&Ty::InferenceVar(var), ty @ &Ty::Dyn(_))
            | (ty @ &Ty::Dyn(_), &Ty::InferenceVar(var))
            | (&Ty::InferenceVar(var), ty @ &Ty::ForAll(_))
            | (ty @ &Ty::ForAll(_), &Ty::InferenceVar(var)) => self.unify_var_ty(var, ty),

            // Unifying `forall<X> { T }` with some other forall type `forall<X> { U }`
            (&Ty::ForAll(ref quantified_ty1), &Ty::ForAll(ref quantified_ty2)) => {
                self.unify_forall_tys(quantified_ty1, quantified_ty2)
            }

            // Unifying `forall<X> { T }` with some other type `U`
            (&Ty::ForAll(ref quantified_ty), other_ty @ &Ty::Apply(_))
            | (&Ty::ForAll(ref quantified_ty), other_ty @ &Ty::Dyn(_))
            | (&Ty::ForAll(ref quantified_ty), other_ty @ &Ty::Opaque(_))
            | (other_ty @ &Ty::Apply(_), &Ty::ForAll(ref quantified_ty))
            | (other_ty @ &Ty::Dyn(_), &Ty::ForAll(ref quantified_ty))
            | (other_ty @ &Ty::Opaque(_), &Ty::ForAll(ref quantified_ty)) => {
                self.unify_forall_other(quantified_ty, other_ty)
            }

            (&Ty::Apply(ref apply1), &Ty::Apply(ref apply2)) => {
                // Cannot unify (e.g.) some struct type `Foo` and some struct type `Bar`
                if apply1.name != apply2.name {
                    return Err(NoSolution);
                }

                Zip::zip_with(self, &apply1.parameters, &apply2.parameters)
            }

            // Cannot unify (e.g.) some struct type `Foo` and an `impl Trait` type
            (&Ty::Apply(_), &Ty::Opaque(_)) | (&Ty::Opaque(_), &Ty::Apply(_)) => {
                return Err(NoSolution);
            }

            // Cannot unify (e.g.) some struct type `Foo` and a `dyn Trait` type
            (&Ty::Apply(_), &Ty::Dyn(_)) | (&Ty::Dyn(_), &Ty::Apply(_)) => {
                return Err(NoSolution);
            }

            // Cannot unify (e.g.) some `dyn Trait` and some `impl Trait` type
            (&Ty::Dyn(..), &Ty::Opaque(..)) | (&Ty::Opaque(..), &Ty::Dyn(..)) => {
                return Err(NoSolution);
            }

            (&Ty::Opaque(ref qwc1), &Ty::Opaque(ref qwc2))
            | (&Ty::Dyn(ref qwc1), &Ty::Dyn(ref qwc2)) => Zip::zip_with(self, qwc1, qwc2),

            // Unifying an associated type projection `<T as
            // Trait>::Item` with some other type `U`.
            (ty @ &Ty::Apply(_), &Ty::Projection(ref proj))
            | (ty @ &Ty::ForAll(_), &Ty::Projection(ref proj))
            | (ty @ &Ty::InferenceVar(_), &Ty::Projection(ref proj))
            | (ty @ &Ty::Dyn(_), &Ty::Projection(ref proj))
            | (ty @ &Ty::Opaque(_), &Ty::Projection(ref proj))
            | (&Ty::Projection(ref proj), ty @ &Ty::Projection(_))
            | (&Ty::Projection(ref proj), ty @ &Ty::Apply(_))
            | (&Ty::Projection(ref proj), ty @ &Ty::ForAll(_))
            | (&Ty::Projection(ref proj), ty @ &Ty::InferenceVar(_))
            | (&Ty::Projection(ref proj), ty @ &Ty::Dyn(_))
            | (&Ty::Projection(ref proj), ty @ &Ty::Opaque(_)) => {
                self.unify_projection_ty(proj, ty)
            }

            (Ty::BoundVar(_), _) | (_, Ty::BoundVar(_)) => panic!(
                "unification encountered bound variable: a={:?} b={:?}",
                a, b
            ),
        }
    }

    fn unify_forall_tys(&mut self, ty1: &QuantifiedTy, ty2: &QuantifiedTy) -> Fallible<()> {
        // for<'a...> T == for<'b...> U
        //
        // if:
        //
        // for<'a...> exists<'b...> T == U &&
        // for<'b...> exists<'a...> T == U
        //
        // Here we only check for<'a...> exists<'b...> T == U,
        // can someone smart comment why this is sufficient?

        debug!("unify_forall_tys({:?}, {:?})", ty1, ty2);

        let ui = self.table.new_universe();
        let lifetimes1: Vec<_> = (0..ty1.num_binders)
            .map(|idx| Lifetime::Placeholder(PlaceholderIndex { ui, idx }).cast())
            .collect();

        let max_universe = self.table.max_universe;
        let lifetimes2: Vec<_> = (0..ty2.num_binders)
            .map(|_| self.table.new_variable(max_universe).to_lifetime().cast())
            .collect();

        let ty1 = ty1.substitute(&lifetimes1);
        let ty2 = ty2.substitute(&lifetimes2);
        debug!("unify_forall_tys: ty1 = {:?}", ty1);
        debug!("unify_forall_tys: ty2 = {:?}", ty2);

        self.sub_unify(ty1, ty2)
    }

    /// Unify an associated type projection `proj` like `<T as Trait>::Item` with some other
    /// type `ty` (which might also be a projection). Creates a goal like
    ///
    /// ```notrust
    /// ProjectionEq(<T as Trait>::Item = U)
    /// ```
    fn unify_projection_ty(&mut self, proj: &ProjectionTy, ty: &Ty) -> Fallible<()> {
        Ok(self.goals.push(InEnvironment::new(
            self.environment,
            ProjectionEq {
                projection: proj.clone(),
                ty: ty.clone(),
            }
            .cast(),
        )))
    }

    /// Unifying `forall<X> { T }` with some other type `U` --
    /// to do so, we create a fresh placeholder `P` for `X` and
    /// see if `[X/Px] T` can be unified with `U`. This should
    /// almost never be true, actually, unless `X` is unused.
    fn unify_forall_other(&mut self, ty1: &QuantifiedTy, ty2: &Ty) -> Fallible<()> {
        let ui = self.table.new_universe();
        let lifetimes1: Vec<_> = (0..ty1.num_binders)
            .map(|idx| Lifetime::Placeholder(PlaceholderIndex { ui, idx }).cast())
            .collect();

        let ty1 = ty1.substitute(&lifetimes1);
        let ty2 = ty2.clone();

        self.sub_unify(ty1, ty2)
    }

    /// Unify an inference variable `var` with some non-inference
    /// variable `ty`, just bind `var` to `ty`. But we must enforce two conditions:
    ///
    /// - `var` does not appear inside of `ty` (the standard `OccursCheck`)
    /// - `ty` does not reference anything in a lifetime that could not be named in `var`
    ///   (the extended `OccursCheck` created to handle universes)
    fn unify_var_ty(&mut self, var: InferenceVar, ty: &Ty) -> Fallible<()> {
        debug!("unify_var_ty(var={:?}, ty={:?})", var, ty);

        let var = EnaVariable::from(var);

        // Determine the universe index associated with this
        // variable. This is basically a count of the number of
        // `forall` binders that had been introduced at the point
        // this variable was created -- though it may change over time
        // as the variable is unified.
        let universe_index = self.table.universe_of_unbound_var(var);

        let ty1 = ty.fold_with(&mut OccursCheck::new(self, var, universe_index), 0)?;

        self.table
            .unify
            .unify_var_value(var, InferenceValue::from(ty1.clone()))
            .unwrap();
        debug!("unify_var_ty: var {:?} set to {:?}", var, ty1);

        Ok(())
    }

    fn unify_lifetime_lifetime(&mut self, a: &Lifetime, b: &Lifetime) -> Fallible<()> {
        if let Some(n_a) = self.table.normalize_lifetime(a) {
            return self.unify_lifetime_lifetime(&n_a, b);
        } else if let Some(n_b) = self.table.normalize_lifetime(b) {
            return self.unify_lifetime_lifetime(a, &n_b);
        }

        debug_heading!("unify_lifetime_lifetime({:?}, {:?})", a, b);

        match (a, b) {
            (&Lifetime::InferenceVar(var_a), &Lifetime::InferenceVar(var_b)) => {
                let var_a = EnaVariable::from(var_a);
                let var_b = EnaVariable::from(var_b);
                debug!(
                    "unify_lifetime_lifetime: var_a={:?} var_b={:?}",
                    var_a, var_b
                );
                self.table.unify.unify_var_var(var_a, var_b).unwrap();
                Ok(())
            }

            (&Lifetime::InferenceVar(var), &Lifetime::Placeholder(idx))
            | (&Lifetime::Placeholder(idx), &Lifetime::InferenceVar(var)) => {
                let var = EnaVariable::from(var);
                let var_ui = self.table.universe_of_unbound_var(var);
                if var_ui.can_see(idx.ui) {
                    debug!(
                        "unify_lifetime_lifetime: {:?} in {:?} can see {:?}; unifying",
                        var, var_ui, idx.ui
                    );
                    let v = Lifetime::Placeholder(idx);
                    self.table
                        .unify
                        .unify_var_value(var, InferenceValue::from(v))
                        .unwrap();
                    Ok(())
                } else {
                    debug!(
                        "unify_lifetime_lifetime: {:?} in {:?} cannot see {:?}; pushing constraint",
                        var, var_ui, idx.ui
                    );
                    Ok(self.push_lifetime_eq_constraint(*a, *b))
                }
            }

            (&Lifetime::Placeholder(_), &Lifetime::Placeholder(_)) => {
                if a != b {
                    Ok(self.push_lifetime_eq_constraint(*a, *b))
                } else {
                    Ok(())
                }
            }

            (Lifetime::BoundVar(_), _) | (_, Lifetime::BoundVar(_)) => panic!(
                "unification encountered bound variable: a={:?} b={:?}",
                a, b
            ),
        }
    }

    fn push_lifetime_eq_constraint(&mut self, a: Lifetime, b: Lifetime) {
        self.constraints.push(InEnvironment::new(
            self.environment,
            Constraint::LifetimeEq(a, b),
        ));
    }
}

impl<'t> Zipper for Unifier<'t> {
    fn zip_tys(&mut self, a: &Ty, b: &Ty) -> Fallible<()> {
        self.unify_ty_ty(a, b)
    }

    fn zip_lifetimes(&mut self, a: &Lifetime, b: &Lifetime) -> Fallible<()> {
        self.unify_lifetime_lifetime(a, b)
    }

    fn zip_binders<T>(&mut self, _: &Binders<T>, _: &Binders<T>) -> Fallible<()>
    where
        T: Zip + Fold<Result = T>,
    {
        panic!("cannot unify things with binders (other than types)")
    }
}

struct OccursCheck<'u, 't: 'u> {
    unifier: &'u mut Unifier<'t>,
    var: EnaVariable,
    universe_index: UniverseIndex,
}

impl<'u, 't> OccursCheck<'u, 't> {
    fn new(unifier: &'u mut Unifier<'t>, var: EnaVariable, universe_index: UniverseIndex) -> Self {
        OccursCheck {
            unifier,
            var,
            universe_index,
        }
    }
}

impl<'u, 't> DefaultTypeFolder for OccursCheck<'u, 't> {}

impl<'u, 't> PlaceholderFolder for OccursCheck<'u, 't> {
    fn fold_free_placeholder_ty(
        &mut self,
        universe: PlaceholderIndex,
        _binders: usize,
    ) -> Fallible<Ty> {
        if self.universe_index < universe.ui {
            Err(NoSolution)
        } else {
            Ok(universe.to_ty()) // no need to shift, not relative to depth
        }
    }

    fn fold_free_placeholder_lifetime(
        &mut self,
        ui: PlaceholderIndex,
        _binders: usize,
    ) -> Fallible<Lifetime> {
        if self.universe_index < ui.ui {
            // Scenario is like:
            //
            // exists<T> forall<'b> ?T = Foo<'b>
            //
            // unlike with a type variable, this **might** be
            // ok.  Ultimately it depends on whether the
            // `forall` also introduced relations to lifetimes
            // nameable in T. To handle that, we introduce a
            // fresh region variable `'x` in same universe as `T`
            // and add a side-constraint that `'x = 'b`:
            //
            // exists<'x> forall<'b> ?T = Foo<'x>, where 'x = 'b

            let tick_x = self.unifier.table.new_variable(self.universe_index);
            self.unifier
                .push_lifetime_eq_constraint(tick_x.to_lifetime(), ui.to_lifetime());
            Ok(tick_x.to_lifetime())
        } else {
            // If the `ui` is higher than `self.universe_index`, then we can name
            // this lifetime, no problem.
            Ok(ui.to_lifetime()) // no need to shift, not relative to depth
        }
    }
}

impl<'u, 't> InferenceFolder for OccursCheck<'u, 't> {
    fn fold_inference_ty(&mut self, var: InferenceVar, _binders: usize) -> Fallible<Ty> {
        let var = EnaVariable::from(var);
        match self.unifier.table.unify.probe_value(var) {
            // If this variable already has a value, fold over that value instead.
            InferenceValue::Bound(normalized_ty) => {
                let normalized_ty = normalized_ty.ty().unwrap();
                let normalized_ty = normalized_ty.fold_with(self, 0)?;
                assert!(!normalized_ty.needs_shift());
                Ok(normalized_ty)
            }

            // Otherwise, check the universe of the variable, and also
            // check for cycles with `self.var` (which this will soon
            // become the value of).
            InferenceValue::Unbound(ui) => {
                if self.unifier.table.unify.unioned(var, self.var) {
                    return Err(NoSolution);
                }

                if self.universe_index < ui {
                    // Scenario is like:
                    //
                    // ?A = foo(?B)
                    //
                    // where ?A is in universe 0 and ?B is in universe 1.
                    // This is OK, if ?B is promoted to universe 0.
                    self.unifier
                        .table
                        .unify
                        .unify_var_value(var, InferenceValue::Unbound(self.universe_index))
                        .unwrap();
                }

                Ok(var.to_ty())
            }
        }
    }

    fn fold_inference_lifetime(&mut self, var: InferenceVar, binders: usize) -> Fallible<Lifetime> {
        // a free existentially bound region; find the
        // inference variable it corresponds to
        let var = EnaVariable::from(var);
        match self.unifier.table.unify.probe_value(var) {
            InferenceValue::Unbound(ui) => {
                if self.universe_index < ui {
                    // Scenario is like:
                    //
                    // exists<T> forall<'b> exists<'a> ?T = Foo<'a>
                    //
                    // where ?A is in universe 0 and `'b` is in universe 1.
                    // This is OK, if `'b` is promoted to universe 0.
                    self.unifier
                        .table
                        .unify
                        .unify_var_value(var, InferenceValue::Unbound(self.universe_index))
                        .unwrap();
                }
                Ok(var.to_lifetime())
            }

            InferenceValue::Bound(l) => {
                let l = l.lifetime().unwrap();
                let l = l.fold_with(self, binders)?;
                assert!(!l.needs_shift());
                Ok(l.clone())
            }
        }
    }
}

impl<'u, 't> DefaultFreeVarFolder for OccursCheck<'u, 't> {
    fn forbid() -> bool {
        true
    }
}
