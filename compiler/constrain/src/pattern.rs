use crate::builtins;
use crate::expr::{constrain_expr, Env};
use roc_can::constraint::Constraint;
use roc_can::expected::{Expected, PExpected};
use roc_can::pattern::Pattern::{self, *};
use roc_can::pattern::{DestructType, RecordDestruct};
use roc_collections::all::{Index, SendMap};
use roc_module::ident::Lowercase;
use roc_module::symbol::Symbol;
use roc_region::all::{Located, Region};
use roc_types::subs::Variable;
use roc_types::types::{Category, PReason, PatternCategory, Reason, RecordField, Type};

pub struct PatternState {
    pub headers: SendMap<Symbol, Located<Type>>,
    pub vars: Vec<Variable>,
    pub constraints: Vec<Constraint>,
}

/// If there is a type annotation, the pattern state headers can be optimized by putting the
/// annotation in the headers. Normally
///
/// x = 4
///
/// Would add `x => <42>` to the headers (i.e., symbol points to a type variable). If the
/// definition has an annotation, we instead now add `x => Int`.
pub fn headers_from_annotation(
    pattern: &Pattern,
    annotation: &Located<Type>,
) -> Option<SendMap<Symbol, Located<Type>>> {
    let mut headers = SendMap::default();
    // Check that the annotation structurally agrees with the pattern, preventing e.g. `{ x, y } : Int`
    // in such incorrect cases we don't put the full annotation in headers, just a variable, and let
    // inference generate a proper error.
    let is_structurally_valid = headers_from_annotation_help(pattern, annotation, &mut headers);

    if is_structurally_valid {
        Some(headers)
    } else {
        None
    }
}

fn headers_from_annotation_help(
    pattern: &Pattern,
    annotation: &Located<Type>,
    headers: &mut SendMap<Symbol, Located<Type>>,
) -> bool {
    match pattern {
        Identifier(symbol) => {
            headers.insert(*symbol, annotation.clone());
            true
        }
        Underscore
        | Shadowed(_, _)
        | MalformedPattern(_, _)
        | UnsupportedPattern(_)
        | NumLiteral(_, _)
        | IntLiteral(_)
        | FloatLiteral(_)
        | StrLiteral(_) => true,

        RecordDestructure { destructs, .. } => match annotation.value.shallow_dealias() {
            Type::Record(fields, _) => {
                for loc_destruct in destructs {
                    let destruct = &loc_destruct.value;

                    // NOTE: We ignore both Guard and optionality when
                    // determining the type of the assigned def (which is what
                    // gets added to the header here).
                    //
                    // For example, no matter whether it's `{ x } = rec` or
                    // `{ x ? 0 } = rec` or `{ x: 5 } -> ...` in all cases
                    // the type of `x` within the binding itself is the same.
                    if let Some(field_type) = fields.get(&destruct.label) {
                        headers.insert(
                            destruct.symbol,
                            Located::at(annotation.region, field_type.clone().into_inner()),
                        );
                    } else {
                        return false;
                    }
                }
                true
            }
            Type::EmptyRec => destructs.is_empty(),
            _ => false,
        },

        AppliedTag {
            tag_name,
            arguments,
            ..
        } => match annotation.value.shallow_dealias() {
            Type::TagUnion(tags, _) => {
                if let Some((_, arg_types)) = tags.iter().find(|(name, _)| name == tag_name) {
                    if !arguments.len() == arg_types.len() {
                        return false;
                    }

                    arguments
                        .iter()
                        .zip(arg_types.iter())
                        .all(|(arg_pattern, arg_type)| {
                            headers_from_annotation_help(
                                &arg_pattern.1.value,
                                &Located::at(annotation.region, arg_type.clone()),
                                headers,
                            )
                        })
                } else {
                    false
                }
            }
            _ => false,
        },
    }
}

/// This accepts PatternState (rather than returning it) so that the caller can
/// intiialize the Vecs in PatternState using with_capacity
/// based on its knowledge of their lengths.
pub fn constrain_pattern(
    env: &Env,
    pattern: &Pattern,
    region: Region,
    expected: PExpected<Type>,
    state: &mut PatternState,
) {
    match pattern {
        Underscore | UnsupportedPattern(_) | MalformedPattern(_, _) | Shadowed(_, _) => {
            // Neither the _ pattern nor erroneous ones add any constraints.
        }

        Identifier(symbol) => {
            state.headers.insert(
                *symbol,
                Located {
                    region,
                    value: expected.get_type(),
                },
            );
        }

        NumLiteral(var, _) => {
            state.vars.push(*var);

            state.constraints.push(Constraint::Pattern(
                region,
                PatternCategory::Num,
                builtins::builtin_type(Symbol::NUM_NUM, vec![Type::Variable(*var)]),
                expected,
            ));
        }

        IntLiteral(_) => {
            state.constraints.push(Constraint::Pattern(
                region,
                PatternCategory::Float,
                builtins::builtin_type(Symbol::NUM_INT, vec![]),
                expected,
            ));
        }

        FloatLiteral(_) => {
            state.constraints.push(Constraint::Pattern(
                region,
                PatternCategory::Float,
                builtins::builtin_type(Symbol::NUM_FLOAT, vec![]),
                expected,
            ));
        }

        StrLiteral(_) => {
            state.constraints.push(Constraint::Pattern(
                region,
                PatternCategory::Str,
                builtins::str_type(),
                expected,
            ));
        }

        RecordDestructure {
            whole_var,
            ext_var,
            destructs,
        } => {
            state.vars.push(*whole_var);
            state.vars.push(*ext_var);
            let ext_type = Type::Variable(*ext_var);

            let mut field_types: SendMap<Lowercase, RecordField<Type>> = SendMap::default();

            for Located {
                value:
                    RecordDestruct {
                        var,
                        label,
                        symbol,
                        typ,
                    },
                ..
            } in destructs
            {
                let pat_type = Type::Variable(*var);
                let expected = PExpected::NoExpectation(pat_type.clone());

                if !state.headers.contains_key(&symbol) {
                    state
                        .headers
                        .insert(*symbol, Located::at(region, pat_type.clone()));
                }

                let field_type = match typ {
                    DestructType::Guard(guard_var, loc_guard) => {
                        state.constraints.push(Constraint::Pattern(
                            region,
                            PatternCategory::PatternGuard,
                            Type::Variable(*guard_var),
                            PExpected::ForReason(
                                PReason::PatternGuard,
                                pat_type.clone(),
                                loc_guard.region,
                            ),
                        ));
                        state.vars.push(*guard_var);

                        constrain_pattern(env, &loc_guard.value, loc_guard.region, expected, state);

                        RecordField::Required(pat_type)
                    }
                    DestructType::Optional(expr_var, loc_expr) => {
                        // Eq(Type, Expected<Type>, Category, Region),
                        let expr_expected = Expected::ForReason(
                            Reason::RecordDefaultField(label.clone()),
                            pat_type.clone(),
                            loc_expr.region,
                        );

                        state.constraints.push(Constraint::Eq(
                            Type::Variable(*expr_var),
                            expr_expected.clone(),
                            Category::DefaultValue(label.clone()),
                            region,
                        ));

                        state.vars.push(*expr_var);

                        let expr_con =
                            constrain_expr(env, loc_expr.region, &loc_expr.value, expr_expected);
                        state.constraints.push(expr_con);

                        RecordField::Optional(pat_type)
                    }
                    DestructType::Required => {
                        // No extra constraints necessary.
                        RecordField::Required(pat_type)
                    }
                };

                field_types.insert(label.clone(), field_type);

                state.vars.push(*var);
            }

            let record_type = Type::Record(field_types, Box::new(ext_type));

            let whole_con = Constraint::Eq(
                Type::Variable(*whole_var),
                Expected::NoExpectation(record_type),
                Category::Storage,
                region,
            );

            let record_con = Constraint::Pattern(
                region,
                PatternCategory::Record,
                Type::Variable(*whole_var),
                expected,
            );

            state.constraints.push(whole_con);
            state.constraints.push(record_con);
        }
        AppliedTag {
            whole_var,
            ext_var,
            tag_name,
            arguments,
        } => {
            let mut argument_types = Vec::with_capacity(arguments.len());
            for (index, (pattern_var, loc_pattern)) in arguments.iter().enumerate() {
                state.vars.push(*pattern_var);

                let pattern_type = Type::Variable(*pattern_var);
                argument_types.push(pattern_type.clone());

                let expected = PExpected::ForReason(
                    PReason::TagArg {
                        tag_name: tag_name.clone(),
                        index: Index::zero_based(index),
                    },
                    pattern_type,
                    region,
                );
                constrain_pattern(env, &loc_pattern.value, loc_pattern.region, expected, state);
            }

            let whole_con = Constraint::Eq(
                Type::Variable(*whole_var),
                Expected::NoExpectation(Type::TagUnion(
                    vec![(tag_name.clone(), argument_types)],
                    Box::new(Type::Variable(*ext_var)),
                )),
                Category::Storage,
                region,
            );

            let tag_con = Constraint::Pattern(
                region,
                PatternCategory::Ctor(tag_name.clone()),
                Type::Variable(*whole_var),
                expected,
            );

            state.vars.push(*whole_var);
            state.vars.push(*ext_var);
            state.constraints.push(whole_con);
            state.constraints.push(tag_con);
        }
    }
}
