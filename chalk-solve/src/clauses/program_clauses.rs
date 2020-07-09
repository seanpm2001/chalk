use crate::clauses::builder::ClauseBuilder;
use crate::rust_ir::*;
use crate::split::Split;
use chalk_ir::cast::{Cast, CastTo, Caster};
use chalk_ir::interner::Interner;
use chalk_ir::*;
use std::iter;
use tracing::instrument;

/// Trait for lowering a given piece of rust-ir source (e.g., an impl
/// or struct definition) into its associated "program clauses" --
/// that is, into the lowered, logical rules that it defines.
pub trait ToProgramClauses<I: Interner> {
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>);
}

impl<I: Interner> ToProgramClauses<I> for ImplDatum<I> {
    /// Given `impl<T: Clone> Clone for Vec<T> { ... }`, generate:
    ///
    /// ```notrust
    /// -- Rule Implemented-From-Impl
    /// forall<T> {
    ///     Implemented(Vec<T>: Clone) :- Implemented(T: Clone).
    /// }
    /// ```
    ///
    /// For a negative impl like `impl... !Clone for ...`, however, we
    /// generate nothing -- this is just a way to *opt out* from the
    /// default auto trait impls, it doesn't have any positive effect
    /// on its own.
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        if self.is_positive() {
            let binders = self.binders.map_ref(|b| (&b.trait_ref, &b.where_clauses));
            builder.push_binders(&binders, |builder, (trait_ref, where_clauses)| {
                builder.push_clause(trait_ref, where_clauses);
            });
        }
    }
}

impl<I: Interner> ToProgramClauses<I> for AssociatedTyValue<I> {
    /// Given the following trait:
    ///
    /// ```notrust
    /// trait Iterable {
    ///     type IntoIter<'a>: 'a;
    /// }
    /// ```
    ///
    /// Then for the following impl:
    /// ```notrust
    /// impl<T> Iterable for Vec<T> where T: Clone {
    ///     type IntoIter<'a> = Iter<'a, T>;
    /// }
    /// ```
    ///
    /// we generate:
    ///
    /// ```notrust
    /// -- Rule Normalize-From-Impl
    /// forall<'a, T> {
    ///     Normalize(<Vec<T> as Iterable>::IntoIter<'a> -> Iter<'a, T>>) :-
    ///         Implemented(T: Clone),  // (1)
    ///         Implemented(Iter<'a, T>: 'a).   // (2)
    /// }
    /// ```
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        let impl_datum = builder.db.impl_datum(self.impl_id);
        let associated_ty = builder.db.associated_ty_data(self.associated_ty_id);

        builder.push_binders(&self.value, |builder, assoc_ty_value| {
            let all_parameters = builder.placeholders_in_scope().to_vec();

            // Get the projection for this associated type:
            //
            // * `impl_params`: `[!T]`
            // * `projection`: `<Vec<!T> as Iterable>::Iter<'!a>`
            let (impl_params, projection) = builder
                .db
                .impl_parameters_and_projection_from_associated_ty_value(&all_parameters, self);

            // Assemble the full list of conditions for projection to be valid.
            // This comes in two parts, marked as (1) and (2) in doc above:
            //
            // 1. require that the where clauses from the impl apply
            let interner = builder.db.interner();
            let impl_where_clauses = impl_datum
                .binders
                .map_ref(|b| &b.where_clauses)
                .into_iter()
                .map(|wc| wc.substitute(interner, impl_params));

            // 2. any where-clauses from the `type` declaration in the trait: the
            //    parameters must be substituted with those of the impl
            let assoc_ty_where_clauses = associated_ty
                .binders
                .map_ref(|b| &b.where_clauses)
                .into_iter()
                .map(|wc| wc.substitute(interner, &projection.substitution));

            // Create the final program clause:
            //
            // ```notrust
            // -- Rule Normalize-From-Impl
            // forall<'a, T> {
            //     Normalize(<Vec<T> as Iterable>::IntoIter<'a> -> Iter<'a, T>>) :-
            //         Implemented(T: Clone),  // (1)
            //         Implemented(Iter<'a, T>: 'a).   // (2)
            // }
            // ```
            builder.push_clause(
                Normalize {
                    alias: AliasTy::Projection(projection.clone()),
                    ty: assoc_ty_value.ty,
                },
                impl_where_clauses.chain(assoc_ty_where_clauses),
            );
        });
    }
}

impl<I: Interner> ToProgramClauses<I> for OpaqueTyDatum<I> {
    /// Given `opaque type T<..>: A + B = HiddenTy;`, we generate:
    ///
    /// ```notrust
    /// AliasEq(T<..> = HiddenTy) :- Reveal.
    /// AliasEq(T<..> = !T<..>).
    /// Implemented(!T<..>: A).
    /// Implemented(!T<..>: B).
    /// ```
    /// where `!T<..>` is the placeholder for the unnormalized type `T<..>`.
    #[instrument(level = "debug", skip(builder))]
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        builder.push_binders(&self.bound, |builder, opaque_ty_bound| {
            let interner = builder.interner();
            let substitution = builder.substitution_in_scope();
            let alias = AliasTy::Opaque(OpaqueTy {
                opaque_ty_id: self.opaque_ty_id,
                substitution: substitution.clone(),
            });

            let alias_placeholder_ty = Ty::new(
                interner,
                ApplicationTy {
                    name: self.opaque_ty_id.cast(interner),
                    substitution,
                },
            );

            // AliasEq(T<..> = HiddenTy) :- Reveal.
            builder.push_clause(
                DomainGoal::Holds(
                    AliasEq {
                        alias: alias.clone(),
                        ty: builder.db.hidden_opaque_type(self.opaque_ty_id),
                    }
                    .cast(interner),
                ),
                iter::once(DomainGoal::Reveal),
            );

            // AliasEq(T<..> = !T<..>).
            builder.push_fact(DomainGoal::Holds(
                AliasEq {
                    alias: alias.clone(),
                    ty: alias_placeholder_ty.clone(),
                }
                .cast(interner),
            ));

            let substitution = Substitution::from1(interner, alias_placeholder_ty.clone());
            for bound in opaque_ty_bound.bounds {
                // Implemented(!T<..>: Bound).
                let bound_with_placeholder_ty = bound.substitute(interner, &substitution);
                builder.push_binders(&bound_with_placeholder_ty, |builder, bound| {
                    builder.push_fact(bound);
                });
            }
        });
    }
}

fn application_ty<I: Interner>(
    builder: &mut ClauseBuilder<'_, I>,
    type_name: impl CastTo<TypeName<I>>,
) -> ApplicationTy<I> {
    let interner = builder.interner();
    ApplicationTy {
        name: type_name.cast(interner),
        substitution: builder.substitution_in_scope(),
    }
}

/// Generates the "well-formed" program clauses for an applicative type
/// with the name `type_name`. For example, given a struct definition:
///
/// ```ignore
/// struct Foo<T: Eq> { }
/// ```
///
/// we would generate the clause:
///
/// ```notrust
/// forall<T> {
///     WF(Foo<T>) :- WF(T: Eq).
/// }
/// ```
///
/// # Parameters
/// - builder -- the clause builder. We assume all the generic types from `Foo` are in scope
/// - type_name -- in our example above, the name `Foo`
/// - where_clauses -- the list of where clauses declared on the type (`T: Eq`, in our example)
fn well_formed_program_clauses<'a, I, Wc>(
    builder: &'a mut ClauseBuilder<'_, I>,
    type_name: impl CastTo<TypeName<I>>,
    where_clauses: Wc,
) where
    I: Interner,
    Wc: Iterator<Item = &'a QuantifiedWhereClause<I>>,
{
    let interner = builder.interner();
    let appl_ty = application_ty(builder, type_name);
    let ty = appl_ty.clone().intern(interner);
    builder.push_clause(
        WellFormed::Ty(ty.clone()),
        where_clauses
            .cloned()
            .map(|qwc| qwc.into_well_formed_goal(interner)),
    );
}

/// Generates the "fully visible" program clauses for an applicative type
/// with the name `type_name`. For example, given a struct definition:
///
/// ```ignore
/// struct Foo<T: Eq> { }
/// ```
///
/// we would generate the clause:
///
/// ```notrust
/// forall<T> {
///     IsFullyVisible(Foo<T>) :- IsFullyVisible(T).
/// }
/// ```
///
/// # Parameters
///
/// - builder -- the clause builder. We assume all the generic types from `Foo` are in scope
/// - type_name -- in our example above, the name `Foo`
fn fully_visible_program_clauses<'a, I>(
    builder: &'a mut ClauseBuilder<'_, I>,
    type_name: impl CastTo<TypeName<I>>,
) where
    I: Interner,
{
    let interner = builder.interner();
    let appl_ty = application_ty(builder, type_name);
    let ty = appl_ty.clone().intern(interner);
    builder.push_clause(
        DomainGoal::IsFullyVisible(ty.clone()),
        appl_ty
            .type_parameters(interner)
            .map(|typ| DomainGoal::IsFullyVisible(typ).cast::<Goal<_>>(interner)),
    );
}

/// Generates the "implied bounds" clauses for an applicative
/// type with the name `type_name`. For example, if `type_name`
/// represents a struct `S` that is declared like:
///
/// ```ignore
/// struct S<T> where T: Eq {  }
/// ```
///
/// then we would generate the rule:
///
/// ```notrust
/// FromEnv(T: Eq) :- FromEnv(S<T>)
/// ```
///
/// # Parameters
///
/// - builder -- the clause builder. We assume all the generic types from `S` are in scope.
/// - type_name -- in our example above, the name `S`
/// - where_clauses -- the list of where clauses declared on the type (`T: Eq`, in our example).
fn implied_bounds_program_clauses<'a, I, Wc>(
    builder: &'a mut ClauseBuilder<'_, I>,
    type_name: impl CastTo<TypeName<I>>,
    where_clauses: Wc,
) where
    I: Interner,
    Wc: Iterator<Item = &'a QuantifiedWhereClause<I>>,
{
    let interner = builder.interner();
    let appl_ty = application_ty(builder, type_name);
    let ty = appl_ty.clone().intern(interner);

    for qwc in where_clauses {
        builder.push_binders(&qwc, |builder, wc| {
            builder.push_clause(wc.into_from_env_goal(interner), Some(ty.clone().from_env()));
        });
    }
}

impl<I: Interner> ToProgramClauses<I> for AdtDatum<I> {
    /// Given the following type definition: `struct Foo<T: Eq> { }`, generate:
    ///
    /// ```notrust
    /// -- Rule WellFormed-Type
    /// forall<T> {
    ///     WF(Foo<T>) :- WF(T: Eq).
    /// }
    ///
    /// -- Rule Implied-Bound-From-Type
    /// forall<T> {
    ///     FromEnv(T: Eq) :- FromEnv(Foo<T>).
    /// }
    ///
    /// forall<T> {
    ///     IsFullyVisible(Foo<T>) :- IsFullyVisible(T).
    /// }
    /// ```
    ///
    /// If the type `Foo` is marked `#[upstream]`, we also generate:
    ///
    /// ```notrust
    /// forall<T> { IsUpstream(Foo<T>). }
    /// ```
    ///
    /// Otherwise, if the type `Foo` is not marked `#[upstream]`, we generate:
    /// ```notrust
    /// forall<T> { IsLocal(Foo<T>). }
    /// ```
    ///
    /// Given an `#[upstream]` type that is also fundamental:
    ///
    /// ```notrust
    /// #[upstream]
    /// #[fundamental]
    /// struct Box<T> {}
    /// ```
    ///
    /// We generate the following clauses:
    ///
    /// ```notrust
    /// forall<T> { IsLocal(Box<T>) :- IsLocal(T). }
    ///
    /// forall<T> { IsUpstream(Box<T>) :- IsUpstream(T). }
    ///
    /// // Generated for both upstream and local fundamental types
    /// forall<T> { DownstreamType(Box<T>) :- DownstreamType(T). }
    /// ```
    ///
    #[instrument(level = "debug", skip(builder))]
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        let interner = builder.interner();
        let binders = self.binders.map_ref(|b| &b.where_clauses);
        let id = self.id;

        builder.push_binders(&binders, |builder, where_clauses| {
            well_formed_program_clauses(builder, id, where_clauses.iter());

            implied_bounds_program_clauses(builder, id, where_clauses.iter());

            fully_visible_program_clauses(builder, id);

            let self_appl_ty = application_ty(builder, id);
            let self_ty = self_appl_ty.clone().intern(interner);

            // Fundamental types often have rules in the form of:
            //     Goal(FundamentalType<T>) :- Goal(T)
            // This macro makes creating that kind of clause easy
            macro_rules! fundamental_rule {
                ($goal:ident) => {
                    // Fundamental types must always have at least one
                    // type parameter for this rule to make any
                    // sense. We currently do not have have any
                    // fundamental types with more than one type
                    // parameter, nor do we know what the behaviour
                    // for that should be. Thus, we are asserting here
                    // that there is only a single type parameter
                    // until the day when someone makes a decision
                    // about how that should behave.
                    assert_eq!(
                        self_appl_ty.len_type_parameters(interner),
                        1,
                        "Only fundamental types with a single parameter are supported"
                    );

                    builder.push_clause(
                        DomainGoal::$goal(self_ty.clone()),
                        Some(DomainGoal::$goal(
                            // This unwrap is safe because we asserted
                            // above for the presence of a type
                            // parameter
                            self_appl_ty.first_type_parameter(interner).unwrap(),
                        )),
                    );
                };
            }

            // Types that are not marked `#[upstream]` satisfy IsLocal(TypeName)
            if !self.flags.upstream {
                // `IsLocalTy(Ty)` depends *only* on whether the type
                // is marked #[upstream] and nothing else
                builder.push_fact(DomainGoal::IsLocal(self_ty.clone()));
            } else if self.flags.fundamental {
                // If a type is `#[upstream]`, but is also
                // `#[fundamental]`, it satisfies IsLocal if and only
                // if its parameters satisfy IsLocal
                fundamental_rule!(IsLocal);
                fundamental_rule!(IsUpstream);
            } else {
                // The type is just upstream and not fundamental
                builder.push_fact(DomainGoal::IsUpstream(self_ty.clone()));
            }

            if self.flags.fundamental {
                fundamental_rule!(DownstreamType);
            }
        });
    }
}

impl<I: Interner> ToProgramClauses<I> for FnDefDatum<I> {
    /// Given the following function definition: `fn bar<T>() where T: Eq`, generate:
    ///
    /// ```notrust
    /// -- Rule WellFormed-Type
    /// forall<T> {
    ///     WF(bar<T>) :- WF(T: Eq)
    /// }
    ///
    /// -- Rule Implied-Bound-From-Type
    /// forall<T> {
    ///     FromEnv(T: Eq) :- FromEnv(bar<T>).
    /// }
    ///
    /// forall<T> {
    ///     IsFullyVisible(bar<T>) :- IsFullyVisible(T).
    /// }
    /// ```
    #[instrument(level = "debug", skip(builder))]
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        let binders = self.binders.map_ref(|b| &b.where_clauses);
        let id = self.id;

        builder.push_binders(&binders, |builder, where_clauses| {
            well_formed_program_clauses(builder, id, where_clauses.iter());

            implied_bounds_program_clauses(builder, id, where_clauses.iter());

            fully_visible_program_clauses(builder, id);
        });
    }
}

impl<I: Interner> ToProgramClauses<I> for TraitDatum<I> {
    /// Given the following trait declaration: `trait Ord<T> where Self: Eq<T> { ... }`, generate:
    ///
    /// ```notrust
    /// -- Rule WellFormed-TraitRef
    /// forall<Self, T> {
    ///    WF(Self: Ord<T>) :- Implemented(Self: Ord<T>), WF(Self: Eq<T>).
    /// }
    /// ```
    ///
    /// and the reverse rules:
    ///
    /// ```notrust
    /// -- Rule Implemented-From-Env
    /// forall<Self, T> {
    ///    (Self: Ord<T>) :- FromEnv(Self: Ord<T>).
    /// }
    ///
    /// -- Rule Implied-Bound-From-Trait
    /// forall<Self, T> {
    ///     FromEnv(Self: Eq<T>) :- FromEnv(Self: Ord<T>).
    /// }
    /// ```
    ///
    /// As specified in the orphan rules, if a trait is not marked `#[upstream]`, the current crate
    /// can implement it for any type. To represent that, we generate:
    ///
    /// ```notrust
    /// // `Ord<T>` would not be `#[upstream]` when compiling `std`
    /// forall<Self, T> { LocalImplAllowed(Self: Ord<T>). }
    /// ```
    ///
    /// For traits that are `#[upstream]` (i.e. not in the current crate), the orphan rules dictate
    /// that impls are allowed as long as at least one type parameter is local and each type
    /// prior to that is fully visible. That means that each type prior to the first local
    /// type cannot contain any of the type parameters of the impl.
    ///
    /// This rule is fairly complex, so we expand it and generate a program clause for each
    /// possible case. This is represented as follows:
    ///
    /// ```notrust
    /// // for `#[upstream] trait Foo<T, U, V> where Self: Eq<T> { ... }`
    /// forall<Self, T, U, V> {
    ///     LocalImplAllowed(Self: Foo<T, U, V>) :- IsLocal(Self).
    /// }
    ///
    /// forall<Self, T, U, V> {
    ///     LocalImplAllowed(Self: Foo<T, U, V>) :-
    ///         IsFullyVisible(Self),
    ///         IsLocal(T).
    /// }
    ///
    /// forall<Self, T, U, V> {
    ///     LocalImplAllowed(Self: Foo<T, U, V>) :-
    ///         IsFullyVisible(Self),
    ///         IsFullyVisible(T),
    ///         IsLocal(U).
    /// }
    ///
    /// forall<Self, T, U, V> {
    ///     LocalImplAllowed(Self: Foo<T, U, V>) :-
    ///         IsFullyVisible(Self),
    ///         IsFullyVisible(T),
    ///         IsFullyVisible(U),
    ///         IsLocal(V).
    /// }
    /// ```
    ///
    /// The overlap check uses compatible { ... } mode to ensure that it accounts for impls that
    /// may exist in some other *compatible* world. For every upstream trait, we add a rule to
    /// account for the fact that upstream crates are able to compatibly add impls of upstream
    /// traits for upstream types.
    ///
    /// ```notrust
    /// // For `#[upstream] trait Foo<T, U, V> where Self: Eq<T> { ... }`
    /// forall<Self, T, U, V> {
    ///     Implemented(Self: Foo<T, U, V>) :-
    ///         Implemented(Self: Eq<T>), // where clauses
    ///         Compatible,               // compatible modality
    ///         IsUpstream(Self),
    ///         IsUpstream(T),
    ///         IsUpstream(U),
    ///         IsUpstream(V),
    ///         CannotProve.              // returns ambiguous
    /// }
    /// ```
    ///
    /// In certain situations, this is too restrictive. Consider the following code:
    ///
    /// ```notrust
    /// /* In crate std */
    /// trait Sized { }
    /// struct str { }
    ///
    /// /* In crate bar (depends on std) */
    /// trait Bar { }
    /// impl Bar for str { }
    /// impl<T> Bar for T where T: Sized { }
    /// ```
    ///
    /// Here, because of the rules we've defined, these two impls overlap. The std crate is
    /// upstream to bar, and thus it is allowed to compatibly implement Sized for str. If str
    /// can implement Sized in a compatible future, these two impls definitely overlap since the
    /// second impl covers all types that implement Sized.
    ///
    /// The solution we've got right now is to mark Sized as "fundamental" when it is defined.
    /// This signals to the Rust compiler that it can rely on the fact that str does not
    /// implement Sized in all contexts. A consequence of this is that we can no longer add an
    /// implementation of Sized compatibly for str. This is the trade off you make when defining
    /// a fundamental trait.
    ///
    /// To implement fundamental traits, we simply just do not add the rule above that allows
    /// upstream types to implement upstream traits. Fundamental traits are not allowed to
    /// compatibly do that.
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        let interner = builder.interner();
        let binders = self.binders.map_ref(|b| &b.where_clauses);
        builder.push_binders(&binders, |builder, where_clauses| {
            let trait_ref = chalk_ir::TraitRef {
                trait_id: self.id,
                substitution: builder.substitution_in_scope(),
            };

            builder.push_clause(
                trait_ref.clone().well_formed(),
                where_clauses
                    .iter()
                    .cloned()
                    .map(|qwc| qwc.into_well_formed_goal(interner))
                    .casted::<Goal<_>>(interner)
                    .chain(Some(trait_ref.clone().cast(interner))),
            );

            // The number of parameters will always be at least 1
            // because of the Self parameter that is automatically
            // added to every trait. This is important because
            // otherwise the added program clauses would not have any
            // conditions.
            let type_parameters: Vec<_> = trait_ref.type_parameters(interner).collect();

            // Drop trait can't have downstream implementation because it can only
            // be implemented with the same genericity as the struct definition,
            // i.e. Drop implementation for `struct S<T: Eq> {}` is forced to be
            // `impl Drop<T: Eq> for S<T> { ... }`. That means that orphan rules
            // prevent Drop from being implemented in downstream crates.
            if self.well_known != Some(WellKnownTrait::Drop) {
                // Add all cases for potential downstream impls that could exist
                for i in 0..type_parameters.len() {
                    builder.push_clause(
                        trait_ref.clone(),
                        where_clauses
                            .iter()
                            .cloned()
                            .casted(interner)
                            .chain(iter::once(DomainGoal::Compatible.cast(interner)))
                            .chain((0..i).map(|j| {
                                DomainGoal::IsFullyVisible(type_parameters[j].clone())
                                    .cast(interner)
                            }))
                            .chain(iter::once(
                                DomainGoal::DownstreamType(type_parameters[i].clone())
                                    .cast(interner),
                            ))
                            .chain(iter::once(GoalData::CannotProve.intern(interner))),
                    );
                }
            }

            // Orphan rules:
            if !self.flags.upstream {
                // Impls for traits declared locally always pass the impl rules
                builder.push_fact(DomainGoal::LocalImplAllowed(trait_ref.clone()));
            } else {
                // Impls for remote traits must have a local type in the right place
                for i in 0..type_parameters.len() {
                    builder.push_clause(
                        DomainGoal::LocalImplAllowed(trait_ref.clone()),
                        (0..i)
                            .map(|j| DomainGoal::IsFullyVisible(type_parameters[j].clone()))
                            .chain(Some(DomainGoal::IsLocal(type_parameters[i].clone()))),
                    );
                }
            }

            // Fundamental traits can be reasoned about negatively without any ambiguity, so no
            // need for this rule if the trait is fundamental.
            if !self.flags.fundamental {
                builder.push_clause(
                    trait_ref.clone(),
                    where_clauses
                        .iter()
                        .cloned()
                        .casted(interner)
                        .chain(iter::once(DomainGoal::Compatible.cast(interner)))
                        .chain(
                            trait_ref
                                .type_parameters(interner)
                                .map(|ty| DomainGoal::IsUpstream(ty).cast(interner)),
                        )
                        .chain(iter::once(GoalData::CannotProve.intern(interner))),
                );
            }

            // Reverse implied bound rules: given (e.g.) `trait Foo: Bar + Baz`,
            // we create rules like:
            //
            // ```
            // FromEnv(T: Bar) :- FromEnv(T: Foo)
            // ```
            //
            // and
            //
            // ```
            // FromEnv(T: Baz) :- FromEnv(T: Foo)
            // ```
            for qwc in &where_clauses {
                builder.push_binders(qwc, |builder, wc| {
                    builder.push_clause(
                        wc.into_from_env_goal(interner),
                        Some(trait_ref.clone().from_env()),
                    );
                });
            }

            // Finally, for every trait `Foo` we make a rule
            //
            // ```
            // Implemented(T: Foo) :- FromEnv(T: Foo)
            // ```
            builder.push_clause(trait_ref.clone(), Some(trait_ref.clone().from_env()));
        });
    }
}

impl<I: Interner> ToProgramClauses<I> for AssociatedTyDatum<I> {
    /// For each associated type, we define the "projection
    /// equality" rules. There are always two; one for a successful normalization,
    /// and one for the "fallback" notion of equality.
    ///
    /// Given: (here, `'a` and `T` represent zero or more parameters)
    ///
    /// ```notrust
    /// trait Foo {
    ///     type Assoc<'a, T>: Bounds where WC;
    /// }
    /// ```
    ///
    /// we generate the 'fallback' rule:
    ///
    /// ```notrust
    /// -- Rule AliasEq-Placeholder
    /// forall<Self, 'a, T> {
    ///     AliasEq(<Self as Foo>::Assoc<'a, T> = (Foo::Assoc<'a, T>)<Self>).
    /// }
    /// ```
    ///
    /// and
    ///
    /// ```notrust
    /// -- Rule AliasEq-Normalize
    /// forall<Self, 'a, T, U> {
    ///     AliasEq(<T as Foo>::Assoc<'a, T> = U) :-
    ///         Normalize(<T as Foo>::Assoc -> U).
    /// }
    /// ```
    ///
    /// We used to generate an "elaboration" rule like this:
    ///
    /// ```notrust
    /// forall<T> {
    ///     T: Foo :- exists<U> { AliasEq(<T as Foo>::Assoc = U) }.
    /// }
    /// ```
    ///
    /// but this caused problems with the recursive solver. In
    /// particular, whenever normalization is possible, we cannot
    /// solve that projection uniquely, since we can now elaborate
    /// `AliasEq` to fallback *or* normalize it. So instead we
    /// handle this kind of reasoning through the `FromEnv` predicate.
    ///
    /// We also generate rules specific to WF requirements and implied bounds:
    ///
    /// ```notrust
    /// -- Rule WellFormed-AssocTy
    /// forall<Self, 'a, T> {
    ///     WellFormed((Foo::Assoc)<Self, 'a, T>) :- WellFormed(Self: Foo), WellFormed(WC).
    /// }
    ///
    /// -- Rule Implied-WC-From-AssocTy
    /// forall<Self, 'a, T> {
    ///     FromEnv(WC) :- FromEnv((Foo::Assoc)<Self, 'a, T>).
    /// }
    ///
    /// -- Rule Implied-Bound-From-AssocTy
    /// forall<Self, 'a, T> {
    ///     FromEnv(<Self as Foo>::Assoc<'a,T>: Bounds) :- FromEnv(Self: Foo), WC.
    /// }
    ///
    /// -- Rule Implied-Trait-From-AssocTy
    /// forall<Self,'a, T> {
    ///     FromEnv(Self: Foo) :- FromEnv((Foo::Assoc)<Self, 'a,T>).
    /// }
    /// ```
    fn to_program_clauses(&self, builder: &mut ClauseBuilder<'_, I>) {
        let interner = builder.interner();
        let binders = self.binders.map_ref(|b| (&b.where_clauses, &b.bounds));
        builder.push_binders(&binders, |builder, (where_clauses, bounds)| {
            let substitution = builder.substitution_in_scope();

            let projection = ProjectionTy {
                associated_ty_id: self.id,
                substitution: substitution.clone(),
            };
            let projection_ty = AliasTy::Projection(projection.clone()).intern(interner);

            // Retrieve the trait ref embedding the associated type
            let trait_ref = builder.db.trait_ref_from_projection(&projection);

            // Construct an application from the projection. So if we have `<T as Iterator>::Item`,
            // we would produce `(Iterator::Item)<T>`.
            let app_ty: Ty<_> = ApplicationTy {
                name: TypeName::AssociatedType(self.id),
                substitution,
            }
            .intern(interner);

            let projection_eq = AliasEq {
                alias: AliasTy::Projection(projection.clone()),
                ty: app_ty.clone(),
            };

            // Fallback rule. The solver uses this to move between the projection
            // and placeholder type.
            //
            //    forall<Self> {
            //        AliasEq(<Self as Foo>::Assoc = (Foo::Assoc)<Self>).
            //    }
            builder.push_fact_with_priority(projection_eq, None, ClausePriority::Low);

            // Well-formedness of projection type.
            //
            //    forall<Self> {
            //        WellFormed((Foo::Assoc)<Self>) :- WellFormed(Self: Foo), WellFormed(WC).
            //    }
            builder.push_clause(
                WellFormed::Ty(app_ty.clone()),
                iter::once(WellFormed::Trait(trait_ref.clone()).cast::<Goal<_>>(interner)).chain(
                    where_clauses
                        .iter()
                        .cloned()
                        .map(|qwc| qwc.into_well_formed_goal(interner))
                        .casted(interner),
                ),
            );

            // Assuming well-formedness of projection type means we can assume
            // the trait ref as well. Mostly used in function bodies.
            //
            //    forall<Self> {
            //        FromEnv(Self: Foo) :- FromEnv((Foo::Assoc)<Self>).
            //    }
            builder.push_clause(FromEnv::Trait(trait_ref.clone()), Some(app_ty.from_env()));

            // Reverse rule for where clauses.
            //
            //    forall<Self> {
            //        FromEnv(WC) :- FromEnv((Foo::Assoc)<Self>).
            //    }
            //
            // This is really a family of clauses, one for each where clause.
            for qwc in &where_clauses {
                builder.push_binders(qwc, |builder, wc| {
                    builder.push_clause(
                        wc.into_from_env_goal(interner),
                        Some(FromEnv::Ty(app_ty.clone())),
                    );
                });
            }

            // Reverse rule for implied bounds.
            //
            //    forall<Self> {
            //        FromEnv(<Self as Foo>::Assoc: Bounds) :- FromEnv(Self: Foo), WC
            //    }
            for quantified_bound in &bounds {
                builder.push_binders(quantified_bound, |builder, bound| {
                    for wc in bound.into_where_clauses(interner, projection_ty.clone()) {
                        builder.push_clause(
                            wc.into_from_env_goal(interner),
                            iter::once(FromEnv::Trait(trait_ref.clone()).cast::<Goal<_>>(interner))
                                .chain(where_clauses.iter().cloned().casted(interner)),
                        );
                    }
                });
            }

            // add new type parameter U
            builder.push_bound_ty(|builder, ty| {
                // `Normalize(<T as Foo>::Assoc -> U)`
                let normalize = Normalize {
                    alias: AliasTy::Projection(projection.clone()),
                    ty: ty.clone(),
                };

                // `AliasEq(<T as Foo>::Assoc = U)`
                let projection_eq = AliasEq {
                    alias: AliasTy::Projection(projection),
                    ty,
                };

                // Projection equality rule from above.
                //
                //    forall<T, U> {
                //        AliasEq(<T as Foo>::Assoc = U) :-
                //            Normalize(<T as Foo>::Assoc -> U).
                //    }
                builder.push_clause(projection_eq, Some(normalize));
            });
        });
    }
}
