use super::{CallingTiming, Event, FinalEvent, PrototypeArgument, RootReference};

use crate::{
	context::{
		get_value_of_variable, invocation::InvocationContext, CallCheckingBehavior,
		SetPropertyError,
	},
	diagnostics::{TypeStringRepresentation, TDZ},
	events::ApplicationResult,
	features::{
		functions::ThisValue,
		iteration::{self, IterationKind},
		objects::SpecialObjects,
	},
	types::{
		calling::FunctionCallingError,
		functions::SynthesisedArgument,
		generics::substitution::SubstitutionArguments,
		get_constraint, is_type_truthy_falsy,
		printing::print_type,
		properties::{get_property, set_property, PropertyKey, PropertyValue},
		substitute, Constructor, StructureGenerics, TypeId, TypeStore,
	},
	Decidable, Environment, Type,
};

#[derive(Default)]
pub struct ErrorsAndInfo {
	pub errors: Vec<crate::types::calling::FunctionCallingError>,
	pub warnings: Vec<crate::types::calling::InfoDiagnostic>,
}

/// type_arguments mut to add new ones in
#[must_use]
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_events(
	events: &[Event],
	this_value: ThisValue,
	type_arguments: &mut SubstitutionArguments,
	environment: &mut Environment,
	target: &mut InvocationContext,
	types: &mut TypeStore,
	errors: &mut ErrorsAndInfo,
) -> Option<ApplicationResult> {
	crate::utilities::notify!("Applying {:#?}", events);
	for (idx, event) in events.iter().enumerate() {
		match event {
			Event::ReadsReference { reference, reflects_dependency, position } => {
				if let Some(id) = reflects_dependency {
					let value = match reference {
						RootReference::Variable(id) => {
							//  Some(&*type_arguments)
							let value =
								get_value_of_variable(environment, *id, Some(type_arguments));
							if let Some(ty) = value {
								ty
							} else {
								errors.errors.push(
									crate::types::calling::FunctionCallingError::TDZ {
										error: TDZ {
											variable_name: environment
												.get_variable_name(*id)
												.to_owned(),
											position: *position,
										},
										call_site: None,
									},
								);
								TypeId::ERROR_TYPE
							}
						}
						RootReference::This => this_value.get(environment, types, *position),
					};
					type_arguments.set_during_application(*id, value);
				}
			}
			Event::SetsVariable(variable, value, position) => {
				let new_value = substitute(*value, type_arguments, environment, types);

				// TODO temp assigns to many contexts, which is bad.
				// Closures should have an indicator of what they close over #56
				let info = target.get_latest_info(environment);
				for id in &type_arguments.closures {
					info.closure_current_values
						.insert((*id, RootReference::Variable(*variable)), new_value);
				}

				info.events.push(Event::SetsVariable(*variable, new_value, *position));
				info.variable_current_value.insert(*variable, new_value);
			}
			Event::Getter { on, under, reflects_dependency, publicity, position, bind_this } => {
				// let was = on;
				let on = substitute(*on, type_arguments, environment, types);

				// crate::utilities::notify!("was {:?} now {:?}", was, on);

				let under = match under {
					PropertyKey::Type(under) => {
						let ty = substitute(*under, type_arguments, environment, types);
						PropertyKey::from_type(ty, types)
					}
					under @ PropertyKey::String(_) => under.clone(),
				};

				let Some((_, value)) = get_property(
					on,
					*publicity,
					&under,
					None,
					environment,
					target,
					types,
					*position,
					*bind_this,
				) else {
					// TODO getters can fail here
					panic!(
					"could not get property {under:?} at {position:?} on {}, (inference or some checking failed)",
					print_type(on, types, environment, true)
				);
				};

				if let Some(id) = reflects_dependency {
					type_arguments.set_during_application(*id, value);
				}
			}
			Event::Setter { on, under, new, initialization, publicity, position } => {
				// let was = on;
				let on = substitute(*on, type_arguments, environment, types);
				// crate::utilities::notify!("was {:?} now {:?}", was, on);

				let under = match under {
					PropertyKey::Type(under) => {
						let ty = substitute(*under, type_arguments, environment, types);
						PropertyKey::from_type(ty, types)
					}
					under @ PropertyKey::String(_) => under.clone(),
				};

				let new = match new {
					PropertyValue::Value(new) => {
						PropertyValue::Value(substitute(*new, type_arguments, environment, types))
					}
					// For declare property
					PropertyValue::Getter(_) => todo!(),
					PropertyValue::Setter(_) => todo!(),
					// TODO this might be a different thing at some point
					PropertyValue::Deleted => {
						environment.delete_property(on, &under);
						return None;
					}
					PropertyValue::Dependent { .. } => {
						todo!()
					}
				};

				let _gc = environment.as_general_context();

				// crate::utilities::notify!(
				// 	"[Event::Setter] {}[{}] = {}",
				// 	crate::types::printing::print_type(on, types, &gc, true),
				// 	crate::types::printing::print_type(under, types, &gc, true),
				// 	if let Property::Value(new) = new {
				// 		crate::types::printing::print_type(new, types, &gc, true)
				// 	} else {
				// 		format!("{:#?}", new)
				// 	}
				// );

				if *initialization {
					// TODO temp fix for closures
					let on = if let Type::Constructor(Constructor::StructureGenerics(
						StructureGenerics { on, arguments: _ },
					)) = types.get_type_by_id(on)
					{
						*on
					} else {
						on
					};

					target.get_latest_info(environment).register_property(
						on,
						*publicity,
						under.clone(),
						new,
						true,
						*position,
					);
				} else {
					let result = set_property(
						on,
						*publicity,
						&under,
						new.clone(),
						environment,
						target,
						types,
						*position,
					);

					if let Err(err) = result {
						if let SetPropertyError::DoesNotMeetConstraint {
							property_constraint,
							reason: _,
						} = err
						{
							let value_type = if let PropertyValue::Value(id) = new {
								TypeStringRepresentation::from_type_id(
									id,
									environment,
									types,
									false,
								)
							} else {
								todo!()
							};

							errors.errors.push(
							crate::types::calling::FunctionCallingError::SetPropertyConstraint {
								property_type: property_constraint,
								value_type,
								assignment_position: position.unwrap(),
								call_site: None,
							},
						);
						} else {
							unreachable!()
						}
					}
				}
			}
			Event::CallsType {
				on,
				with,
				reflects_dependency,
				timing,
				called_with_new,
				position: _,
			} => {
				let on = substitute(*on, type_arguments, environment, types);

				// crate::utilities::notify!("was {:?} now {:?}", was, on);

				let with = with
					.iter()
					.map(|argument| SynthesisedArgument {
						value: substitute(argument.value, type_arguments, environment, types),
						position: argument.position,
						spread: argument.spread,
					})
					.collect::<Vec<_>>();

				match timing {
					CallingTiming::Synchronous => {
						let result = crate::types::calling::call_type(
							on,
							with,
							&crate::types::calling::CallingInput {
								called_with_new: *called_with_new,
								call_site_type_arguments: None,
								// TODO:
								call_site: source_map::Nullable::NULL,
							},
							environment,
							target,
							types,
						);
						match result {
							Ok(mut result) => {
								errors.warnings.append(&mut result.warnings);
								if let Some(reflects_dependency) = reflects_dependency {
									type_arguments.set_during_application(
										*reflects_dependency,
										result.returned_type,
									);
								}

								// if result.thrown_type != TypeId::NEVER_TYPE {
								// 	// TODO
								// 	// return FinalEvent::Throw {
								// 	// 	thrown: result.thrown_type,
								// 	// 	position: source_map::Nullable::NULL,
								// 	// }
								// 	// .into();
								// }
							}
							Err(mut calling_errors) => {
								crate::utilities::notify!(
									"inference and or checking failed at function"
								);
								errors.errors.append(&mut calling_errors);
								if let Some(reflects_dependency) = reflects_dependency {
									type_arguments.set_during_application(
										*reflects_dependency,
										TypeId::ERROR_TYPE,
									);
								}
							}
						}
					}
					// TODO different
					CallingTiming::QueueTask | CallingTiming::AtSomePointManyTimes => {
						todo!()
						// TODO unsure whether need function id here
						// if let Some(Constant::FunctionReference(function)) =
						// 	environment.get_constant_type(on)
						// {
						// 	match function {
						// 		FunctionPointer::Function(function_id) => {
						// 			environment.tasks_to_run.push((on, *function_id));
						// 		}
						// 		FunctionPointer::AutoConstructor(..)
						// 		| FunctionPointer::Internal(..) => {
						// 			todo!()
						// 		}
						// 	}
						// } else {
						// 	unreachable!("calling something that isn't a function... ?")
						// }
					}
				}
			}
			// TODO extract
			Event::Conditionally { condition, truthy_events, otherwise_events, position } => {
				let condition = substitute(*condition, type_arguments, environment, types);

				let fixed_result = is_type_truthy_falsy(condition, types);
				// crate::utilities::notify!("Condition {:?} {:?}", types.get_type_by_id(condition), result);

				match fixed_result {
					Decidable::Known(result) => {
						let (truthy_events, otherwise_events) =
							(*truthy_events as usize, *otherwise_events as usize);
						let offset = idx + 1;
						let (start, end) = if result {
							(offset, offset + truthy_events)
						} else {
							(offset + truthy_events, offset + truthy_events + otherwise_events)
						};
						let events_to_run = &events[start as usize..end as usize];
						crate::utilities::notify!(
							"(start, end) = {:?}, {:?}",
							(start, end),
							events_to_run
						);
						let result =
							target.new_unconditional_target(|target: &mut InvocationContext| {
								apply_events(
									events_to_run,
									this_value,
									type_arguments,
									environment,
									target,
									types,
									errors,
								)
							});

						if result.is_some() {
							crate::utilities::notify!("Here {:?}", result);
							return result;
						}
					}
					Decidable::Unknown(condition) => {
						todo!()
						// TODO early returns

						// TODO could inject proofs but probably already worked out
						// let (mut truthy_info, truthy_result) =
						// 	target.new_conditional_target(|target: &mut InvocationContext| {
						// 		for event in events_if_truthy.into_vec() {
						// 			let result = apply_event(
						// 				event,
						// 				rest_of_events,
						// 				this_value,
						// 				type_arguments,
						// 				environment,
						// 				target,
						// 				types,
						// 				errors,
						// 			);

						// 			if result.is_some() {
						// 				return result;
						// 			}
						// 		}
						// 		None
						// 	});

						// let (mut otherwise_info, otherwise_result) =
						// 	target.new_conditional_target(|target: &mut InvocationContext| {
						// 		for event in else_events.into_vec() {
						// 			let result = apply_event(
						// 				event,
						// 				rest_of_events,
						// 				this_value,
						// 				type_arguments,
						// 				environment,
						// 				target,
						// 				types,
						// 				errors,
						// 			);

						// 			if result.is_some() {
						// 				return result;
						// 			}
						// 		}
						// 		None
						// 	});

						// // TODO all things that are
						// // - variable and property values (these aren't read from events)
						// // - immutable, mutable, prototypes etc
						// let info = target.get_latest_info(environment);

						// // Merge variable current values conditionally. TODO other info...?
						// for (var, truth) in truthy_info.variable_current_value {
						// 	let entry = info.variable_current_value.entry(var);
						// 	entry.and_modify(|existing| {
						// 		let otherwise_result = otherwise_info
						// 			.variable_current_value
						// 			.remove(&var)
						// 			.unwrap_or(*existing);

						// 		*existing =
						// 			types.new_conditional_type(condition, truth, otherwise_result);
						// 	});
						// }

						// info.events.push(Event::Conditionally {
						// 	condition,
						// 	truthy_events: truthy_info.events.into_boxed_slice(),
						// 	otherwise_events: otherwise_info.events.into_boxed_slice(),
						// 	position,
						// });

						// return if truthy_result.is_none() && otherwise_result.is_none() {
						// 	None
						// } else {
						// 	Some(ApplicationResult::Or {
						// 		on: condition,
						// 		truthy_result: truthy_result.map(Box::new),
						// 		otherwise_result: otherwise_result.map(Box::new),
						// 	})
						// };
					}
				}
			}
			Event::FinalEvent(final_event) => {
				return Some(match final_event {
					FinalEvent::Break { carry, position } => ApplicationResult::Break {
						// TODO is this correct?
						carry: carry.saturating_sub(target.get_iteration_depth()),
						position: *position,
					},
					FinalEvent::Continue { carry, position } => ApplicationResult::Continue {
						// TODO is this correct?
						carry: carry.saturating_sub(target.get_iteration_depth()),
						position: *position,
					},
					FinalEvent::Throw { thrown, position } => {
						let substituted_thrown =
							substitute(*thrown, type_arguments, environment, types);
						if target.in_unconditional() {
							let value = TypeStringRepresentation::from_type_id(
								substituted_thrown,
								// TODO is this okay?
								environment,
								types,
								false,
							);
							errors.errors.push(FunctionCallingError::UnconditionalThrow {
								value,
								call_site: None,
							});
						}
						ApplicationResult::Throw { thrown: substituted_thrown, position: *position }
					}
					FinalEvent::Return { returned, position } => {
						let substituted_returned =
							substitute(*returned, type_arguments, environment, types);
						ApplicationResult::Return {
							returned: substituted_returned,
							position: *position,
						}
					}
				});
			}
			// TODO Needs a position (or not?)
			Event::CreateObject { referenced_in_scope_as, prototype, position: _ } => {
				// TODO
				let is_under_dyn = true;

				let new_object_id = match prototype {
					PrototypeArgument::Yeah(prototype) => {
						let prototype = substitute(*prototype, type_arguments, environment, types);
						target.get_latest_info(environment).new_object(
							Some(prototype),
							types,
							is_under_dyn,
						)
					}
					PrototypeArgument::None => {
						target.get_latest_info(environment).new_object(None, types, is_under_dyn)
					}
					PrototypeArgument::Function(id) => types.register_type(
						crate::Type::SpecialObject(SpecialObjects::Function(*id, this_value)),
					),
				};

				// TODO conditionally if any properties are structurally generic
				// let new_object_id_with_curried_arguments =
				// 	curry_arguments(type_arguments, types, new_object_id);

				// crate::utilities::notify!(
				// 	"Setting {:?} to {:?}",
				// 	referenced_in_scope_as,
				// 	new_object_id_with_curried_arguments
				// );

				if let Some(object_constraint) =
					environment.get_object_constraint(*referenced_in_scope_as)
				{
					environment.add_object_constraints(
						std::iter::once((new_object_id, object_constraint)),
						types,
					);
				}

				type_arguments.set_during_application(*referenced_in_scope_as, new_object_id);
			}
			Event::Iterate { kind, iterate_over, initial } => {
				todo!()
				// let initial = initial
				// 	.into_iter()
				// 	.map(|(id, value)| (id, substitute(value, type_arguments, environment, types)))
				// 	.collect();

				// let kind = match kind {
				// 	IterationKind::Condition { under, postfix_condition } => IterationKind::Condition {
				// 		under: under.map(|under| under.specialise(type_arguments, environment, types)),
				// 		postfix_condition,
				// 	},
				// 	IterationKind::Properties { on, variable } => IterationKind::Properties {
				// 		on: substitute(on, type_arguments, environment, types),
				// 		variable,
				// 	},
				// 	IterationKind::Iterator { on, variable } => IterationKind::Iterator {
				// 		on: substitute(on, type_arguments, environment, types),
				// 		variable,
				// 	},
				// };

				// return iteration::run_iteration_block(
				// 	kind,
				// 	iterate_over.to_vec(),
				// 	iteration::InitialVariablesInput::Calculated(initial),
				// 	type_arguments,
				// 	environment,
				// 	target,
				// 	errors,
				// 	types,
				// );
			}
			Event::ExceptionTrap { investigate, handle, finally: _, trapped_type_id } => {
				todo!()
				// let mut investigate_iter = investigate.to_vec().into_iter();
				// while let Some(event) = investigate_iter.next() {
				// 	let termination = apply_event(
				// 		event,
				// 		&mut investigate_iter,
				// 		this_value,
				// 		type_arguments,
				// 		environment,
				// 		target,
				// 		types,
				// 		errors,
				// 	);

				// 	// if let ApplicationResult::Interrupt(i) = termination {
				// 	// 	match i {
				// 	// 		FinalEvent::Return { .. }
				// 	// 		| FinalEvent::Break { .. }
				// 	// 		| FinalEvent::Continue { .. } => return termination,
				// 	// 		FinalEvent::Throw { thrown, position: _ } => {
				// 	// 			if let Some(trap) = trapped_type_id {
				// 	// 				type_arguments.set_during_application(trap.generic_type, thrown);

				// 	// 				if let Some(_a) = trap.constrained {
				// 	// 					crate::utilities::notify!("TODO check using function");
				// 	// 				}
				// 	// 				let mut handler_iter = handle.to_vec().into_iter();

				// 	// 				while let Some(event) = handler_iter.next() {
				// 	// 					let termination = apply_event(
				// 	// 						event,
				// 	// 						&mut handler_iter,
				// 	// 						this_value,
				// 	// 						type_arguments,
				// 	// 						environment,
				// 	// 						target,
				// 	// 						types,
				// 	// 						errors,
				// 	// 					);
				// 	// 					crate::utilities::notify!("TODO break on last = end");
				// 	// 				}
				// 	// 			} else {
				// 	// 				crate::utilities::notify!("here");
				// 	// 			}
				// 	// 		}
				// 	// 	}
				// 	// } else {
				// 	// 	crate::utilities::notify!("todo {:?}", termination);
				// 	// }
				// }
			}
			Event::RegisterVariable { .. } => {}
		}
	}
	None
}

/// For loops and recursion
///
/// - TODO more might need covering
/// - TODO `_this_value` is not being used
#[allow(clippy::match_same_arms, clippy::used_underscore_binding)]
pub(crate) fn apply_event_unknown(
	event: Event,
	_this_value: ThisValue,
	type_arguments: &mut SubstitutionArguments,
	environment: &mut Environment,
	target: &mut InvocationContext,
	types: &mut TypeStore,
) {
	match event {
		Event::ReadsReference { reflects_dependency, reference, .. } => {
			if let (Some(reflects_dependency), RootReference::Variable(variable)) =
				(reflects_dependency, reference)
			{
				// TODO this is okay for loops, not sure about other cases of this function
				crate::utilities::notify!("Setting loop variable here {:?}", reflects_dependency);
				target
					.get_latest_info(environment)
					.variable_current_value
					.insert(variable, reflects_dependency);
			}
		}
		Event::Getter { reflects_dependency, on, under, publicity, position, bind_this } => {
			// Evaluates getters
			if let Some(reflects_dependency) = reflects_dependency {
				crate::utilities::notify!("Run getters");
				let on = substitute(on, type_arguments, environment, types);
				let under = match under {
					under @ PropertyKey::String(_) => under,
					PropertyKey::Type(under) => {
						PropertyKey::Type(substitute(under, type_arguments, environment, types))
					}
				};
				// get_property(
				// 	on,
				// 	publicity,
				// 	&under,
				// 	None,
				// 	top_environment,
				// 	target,
				// 	types,
				// 	position,
				// 	bind_this,
				// )
			}
		}
		Event::SetsVariable(_variable, _value, _) => {
			crate::utilities::notify!("Here");
			// let new_value = get_constraint(value, types)
			// 	.map(|value| {
			// 		types.register_type(Type::RootPolyType(crate::types::PolyNature::Open(value)))
			// 	})
			// 	.unwrap_or(value);

			// // Don't like this but I think it is okay
			// environment.info.variable_current_value.insert(variable, new_value);
		}
		Event::Setter { on, under, new, initialization: _, publicity, position } => {
			let on = substitute(on, type_arguments, environment, types);
			let new_value = match new {
				PropertyValue::Value(new) => {
					let new = get_constraint(new, types).map_or(new, |value| {
						types.register_type(Type::RootPolyType(crate::types::PolyNature::Open(
							value,
						)))
					});
					PropertyValue::Value(new)
				}
				// TODO
				PropertyValue::Dependent { .. }
				| PropertyValue::Getter(_)
				| PropertyValue::Setter(_)
				| PropertyValue::Deleted => new,
			};
			let under = match under {
				under @ PropertyKey::String(_) => under,
				PropertyKey::Type(under) => {
					PropertyKey::Type(substitute(under, type_arguments, environment, types))
				}
			};

			environment.info.register_property(on, publicity, under, new_value, false, position);
		}
		Event::CallsType { .. } => {
			crate::utilities::notify!("TODO ?");
		}
		Event::Conditionally {
			truthy_events: true_events, otherwise_events: else_events, ..
		} => {
			// TODO think this is correct...?
			// for event in true_events.into_vec() {
			// 	apply_event_unknown(event, _this_value, type_arguments, environment, target, types);
			// }
			// for event in else_events.into_vec() {
			// 	apply_event_unknown(event, _this_value, type_arguments, environment, target, types);
			// }
		}
		Event::CreateObject { .. } => {}
		Event::FinalEvent(FinalEvent::Return { .. }) => {}
		Event::FinalEvent(FinalEvent::Throw { .. }) => {}
		Event::FinalEvent(FinalEvent::Break { .. }) => {
			// TODO conditionally
		}
		Event::FinalEvent(FinalEvent::Continue { .. }) => {
			// TODO conditionally
		}
		Event::Iterate { .. } => {
			// This should be fine?
			// for event in iterate_over.to_vec() {
			// 	apply_event_unknown(
			// 		event,
			// 		this_value,
			// 		type_arguments,
			// 		environment,
			// 		target,
			// 		types,
			// 	)
			// }
			crate::utilities::notify!("Iterate trap anytime");
		}
		Event::ExceptionTrap { .. } => {
			crate::utilities::notify!("Exception trap anytime");
		}
		Event::RegisterVariable { name, position, initial_value } => todo!(),
	}
}
