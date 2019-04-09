use graphql_parser::query as q;
use graphql_parser::schema as s;
use indexmap::IndexMap;
use std::cmp;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ops::Deref;
use std::time::Instant;

use graph::prelude::*;

use crate::prelude::*;
use crate::query::ast as qast;
use crate::schema::ast as sast;
use crate::values::coercion;

/// Contextual information passed around during query execution.
#[derive(Clone)]
pub struct ExecutionContext<'a, R1, R2>
where
    R1: Resolver,
    R2: Resolver,
{
    /// The logger to use.
    pub logger: Logger,

    /// The schema to execute the query against.
    pub schema: &'a Schema,

    /// Introspection data that corresponds to the schema.
    pub introspection_schema: &'a s::Document,

    /// The query to execute.
    pub document: &'a q::Document,

    /// The resolver to use.
    pub resolver: Arc<R1>,

    /// The introspection resolver to use.
    pub introspection_resolver: Arc<R2>,

    /// The current field stack (e.g. allUsers > friends > name).
    pub fields: Vec<&'a q::Field>,

    /// Whether or not we're executing an introspection query
    pub introspecting: bool,

    /// Variable values.
    pub variable_values: Arc<HashMap<q::Name, q::Value>>,

    /// Time at which the query times out.
    pub deadline: Option<Instant>,
}

impl<'a, R1, R2> ExecutionContext<'a, R1, R2>
where
    R1: Resolver,
    R2: Resolver,
{
    /// Creates a derived context for a new field (added to the top of the field stack).
    pub fn for_field(&mut self, field: &'a q::Field) -> Self {
        let mut ctx = self.clone();
        ctx.fields.push(field);
        ctx
    }

    // Helper functions that decide whether to call underlying functions on the
    // regular or on the introspection schema. Only these functions should be
    // making decisions on whether we are introspecting or not
    fn get_schema(&self) -> &s::Document {
        if self.introspecting {
            self.introspection_schema
        } else {
            &self.schema.document
        }
    }

    fn resolve_object(
        &self,
        parent: &Option<q::Value>,
        field: &q::Field,
        field_definition: &s::Field,
        object_type: ObjectOrInterface<'_>,
        arguments: &HashMap<&q::Name, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        if self.introspecting {
            self.introspection_resolver.resolve_object(
                parent,
                field,
                field_definition,
                object_type,
                arguments,
                &BTreeMap::new(), // The introspection schema has no interfaces.
            )
        } else {
            self.resolver.resolve_object(
                parent,
                field,
                field_definition,
                object_type,
                arguments,
                self.schema.types_for_interface(),
            )
        }
    }

    fn resolve_objects(
        &self,
        parent: &Option<q::Value>,
        field: &q::Name,
        field_definition: &s::Field,
        object_type: ObjectOrInterface<'_>,
        arguments: &HashMap<&q::Name, q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        if self.introspecting {
            self.introspection_resolver.resolve_objects(
                parent,
                field,
                field_definition,
                object_type,
                arguments,
                &BTreeMap::new(), // The introspection schema has no interfaces.
            )
        } else {
            self.resolver.resolve_objects(
                parent,
                field,
                field_definition,
                object_type,
                arguments,
                self.schema.types_for_interface(),
            )
        }
    }

    fn resolve_enum_value(
        &self,
        field: &q::Field,
        enum_type: &s::EnumType,
        value: Option<&q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        if self.introspecting {
            self.introspection_resolver
                .resolve_enum_value(field, enum_type, value)
        } else {
            self.resolver.resolve_enum_value(field, enum_type, value)
        }
    }

    fn resolve_enum_values(
        &self,
        field: &q::Field,
        enum_type: &s::EnumType,
        value: Option<&q::Value>,
    ) -> Result<q::Value, Vec<QueryExecutionError>> {
        if self.introspecting {
            self.introspection_resolver
                .resolve_enum_values(field, enum_type, value)
        } else {
            self.resolver.resolve_enum_values(field, enum_type, value)
        }
    }

    fn resolve_scalar_value(
        &self,
        parent_object_type: &s::ObjectType,
        parent: &BTreeMap<String, q::Value>,
        field: &q::Field,
        scalar_type: &s::ScalarType,
        value: Option<&q::Value>,
    ) -> Result<q::Value, QueryExecutionError> {
        if self.introspecting {
            self.introspection_resolver.resolve_scalar_value(
                parent_object_type,
                parent,
                field,
                scalar_type,
                value,
            )
        } else {
            self.resolver.resolve_scalar_value(
                parent_object_type,
                parent,
                field,
                scalar_type,
                value,
            )
        }
    }

    fn resolve_scalar_values(
        &self,
        field: &q::Field,
        scalar_type: &s::ScalarType,
        value: Option<&q::Value>,
    ) -> Result<q::Value, Vec<QueryExecutionError>> {
        if self.introspecting {
            self.introspection_resolver
                .resolve_scalar_values(field, scalar_type, value)
        } else {
            self.resolver
                .resolve_scalar_values(field, scalar_type, value)
        }
    }
}

/// Executes the root selection set of a query.
pub fn execute_root_selection_set<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    selection_set: &'a q::SelectionSet,
    initial_value: &Option<q::Value>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    // Obtain the root Query type and fail if there isn't one
    let query_type = match sast::get_root_query_type(&ctx.schema.document) {
        Some(t) => t,
        None => return Err(vec![QueryExecutionError::NoRootQueryObjectType]),
    };

    // Split the toplevel fields into introspection fields and
    // 'normal' data fields
    let mut errors: Vec<QueryExecutionError> = Vec::new();
    let mut data_set = q::SelectionSet {
        span: selection_set.span.clone(),
        items: Vec::new(),
    };
    let mut intro_set = q::SelectionSet {
        span: selection_set.span.clone(),
        items: Vec::new(),
    };

    for (_, fields) in collect_fields(ctx.clone(), query_type, selection_set, None) {
        if let Some((_, introspecting)) = get_field_type(ctx.clone(), query_type, &fields[0].name) {
            let selections = fields.into_iter().map(|f| q::Selection::Field(f.clone()));
            if introspecting {
                intro_set.items.extend(selections)
            } else {
                data_set.items.extend(selections)
            }
        } else {
            errors.push(QueryExecutionError::UnknownField(
                fields[0].position,
                query_type.name.clone(),
                fields[0].name.clone(),
            ))
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    // Execute the root selection set against the root query type
    if data_set.items.is_empty() {
        // Only introspection
        execute_selection_set(ctx, &intro_set, query_type, initial_value)
    } else if intro_set.items.is_empty() {
        // Only data
        execute_selection_set(ctx, &data_set, query_type, initial_value)
    } else {
        // Both introspection and data
        let mut values =
            execute_selection_set_to_map(ctx.clone(), &data_set, query_type, initial_value)?;
        values.extend(execute_selection_set_to_map(
            ctx,
            &intro_set,
            query_type,
            initial_value,
        )?);
        Ok(q::Value::Object(values))
    }
}

/// Executes a selection set, requiring the result to be of the given object type.
///
/// Allows passing in a parent value during recursive processing of objects and their fields.
pub fn execute_selection_set<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    selection_set: &'a q::SelectionSet,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    Ok(q::Value::Object(execute_selection_set_to_map(
        ctx,
        selection_set,
        object_type,
        object_value,
    )?))
}

fn execute_selection_set_to_map<'a, R1, R2>(
    mut ctx: ExecutionContext<'a, R1, R2>,
    selection_set: &'a q::SelectionSet,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
) -> Result<BTreeMap<String, q::Value>, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    let mut errors: Vec<QueryExecutionError> = Vec::new();
    let mut result_map: BTreeMap<String, q::Value> = BTreeMap::new();

    // Group fields with the same response key, so we can execute them together
    let grouped_field_set = collect_fields(ctx.clone(), object_type, selection_set, None);

    // Process all field groups in order
    for (response_key, fields) in grouped_field_set {
        match ctx.deadline {
            Some(deadline) if deadline < Instant::now() => {
                errors.push(QueryExecutionError::Timeout);
                break;
            }
            _ => (),
        }

        // If the field exists on the object, execute it and add its result to the result map
        if let Some((ref field, introspecting)) =
            get_field_type(ctx.clone(), object_type, &fields[0].name)
        {
            // Push the new field onto the context's field stack
            let mut ctx = ctx.for_field(&fields[0]);

            // Remember whether or not we're introspecting now
            ctx.introspecting = introspecting;

            match execute_field(ctx, object_type, object_value, &fields[0], field, fields) {
                Ok(v) => {
                    result_map.insert(response_key.to_owned(), v);
                }
                Err(mut e) => {
                    errors.append(&mut e);
                }
            };
        } else {
            errors.push(QueryExecutionError::UnknownField(
                fields[0].position,
                object_type.name.clone(),
                fields[0].name.clone(),
            ))
        }
    }

    if errors.is_empty() && !result_map.is_empty() {
        Ok(result_map)
    } else {
        if errors.is_empty() {
            errors.push(QueryExecutionError::EmptySelectionSet(
                object_type.name.clone(),
            ));
        }
        Err(errors)
    }
}

/// Collects fields of a selection set.
pub fn collect_fields<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    selection_set: &'a q::SelectionSet,
    visited_fragments: Option<HashSet<&'a q::Name>>,
) -> IndexMap<&'a String, Vec<&'a q::Field>>
where
    R1: Resolver,
    R2: Resolver,
{
    let mut visited_fragments = visited_fragments.unwrap_or_default();
    let mut grouped_fields: IndexMap<_, Vec<_>> = IndexMap::new();

    // Only consider selections that are not skipped and should be included
    let selections: Vec<_> = selection_set
        .items
        .iter()
        .filter(|selection| !qast::skip_selection(selection, ctx.variable_values.deref()))
        .filter(|selection| qast::include_selection(selection, ctx.variable_values.deref()))
        .collect();

    for selection in selections {
        match selection {
            q::Selection::Field(ref field) => {
                // Obtain the response key for the field
                let response_key = qast::get_response_key(field);

                // Create a field group for this response key on demand and
                // append the selection field to this group.
                grouped_fields.entry(response_key).or_default().push(field);
            }

            q::Selection::FragmentSpread(spread) => {
                // Only consider the fragment if it hasn't already been included,
                // as would be the case if the same fragment spread ...Foo appeared
                // twice in the same selection set
                if !visited_fragments.contains(&spread.fragment_name) {
                    visited_fragments.insert(&spread.fragment_name);

                    // Resolve the fragment using its name and, if it applies, collect
                    // fields for the fragment and group them
                    let fragment_grouped_field_set =
                        qast::get_fragment(&ctx.document, &spread.fragment_name)
                            .and_then(|fragment| {
                                // We have a fragment, only pass it on if it applies to the
                                // current object type
                                if does_fragment_type_apply(
                                    ctx.clone(),
                                    object_type,
                                    &fragment.type_condition,
                                ) {
                                    Some(fragment)
                                } else {
                                    None
                                }
                            })
                            .map(|fragment| {
                                // We have a fragment that applies to the current object type,
                                // collect its fields into response key groups
                                collect_fields(
                                    ctx.clone(),
                                    object_type,
                                    &fragment.selection_set,
                                    Some(visited_fragments.clone()),
                                )
                            });

                    if let Some(grouped_field_set) = fragment_grouped_field_set {
                        // Add all items from each fragments group to the field group
                        // with the corresponding response key
                        for (response_key, mut fragment_group) in grouped_field_set {
                            grouped_fields
                                .entry(response_key)
                                .or_default()
                                .append(&mut fragment_group);
                        }
                    }
                }
            }

            q::Selection::InlineFragment(_) => {
                warn!(ctx.logger, "Inline fragments are not yet implemented")
            }
        };
    }

    grouped_fields
}

/// Determines whether a fragment is applicable to the given object type.
fn does_fragment_type_apply<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    fragment_type: &q::TypeCondition,
) -> bool
where
    R1: Resolver,
    R2: Resolver,
{
    // This is safe to do, as TypeCondition only has a single `On` variant.
    let q::TypeCondition::On(ref name) = fragment_type;

    // Resolve the type the fragment applies to based on its name
    let named_type = sast::get_named_type(ctx.get_schema(), name);

    match named_type {
        // The fragment applies to the object type if its type is the same object type
        Some(s::TypeDefinition::Object(ot)) => object_type == ot,

        // The fragment also applies to the object type if its type is an interface
        // that the object type implements
        Some(s::TypeDefinition::Interface(it)) => object_type
            .implements_interfaces
            .iter()
            .find(|name| name == &&it.name)
            .map(|_| true)
            .unwrap_or(false),

        // The fragment also applies to an object type if its type is a union that
        // the object type is one of the possible types for
        Some(s::TypeDefinition::Union(ut)) => ut
            .types
            .iter()
            .find(|name| name == &&object_type.name)
            .map(|_| true)
            .unwrap_or(false),

        // In all other cases, the fragment does not apply
        _ => false,
    }
}

/// Executes a field.
fn execute_field<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
    field: &'a q::Field,
    field_definition: &s::Field,
    fields: Vec<&'a q::Field>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    coerce_argument_values(&ctx, object_type, field)
        .and_then(|argument_values| {
            resolve_field_value(
                ctx.clone(),
                object_type,
                object_value,
                field,
                field_definition,
                &field_definition.field_type,
                &argument_values,
            )
        })
        .and_then(|value| complete_value(ctx, field, &field_definition.field_type, fields, value))
}

/// Resolves the value of a field.
fn resolve_field_value<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
    field: &q::Field,
    field_definition: &s::Field,
    field_type: &s::Type,
    argument_values: &HashMap<&q::Name, q::Value>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    match field_type {
        s::Type::NonNullType(inner_type) => resolve_field_value(
            ctx,
            object_type,
            object_value,
            field,
            field_definition,
            inner_type.as_ref(),
            argument_values,
        ),

        s::Type::NamedType(ref name) => resolve_field_value_for_named_type(
            ctx,
            object_type,
            object_value,
            field,
            field_definition,
            name,
            argument_values,
        ),

        s::Type::ListType(inner_type) => resolve_field_value_for_list_type(
            ctx,
            object_type,
            object_value,
            field,
            field_definition,
            inner_type.as_ref(),
            argument_values,
        ),
    }
}

/// Resolves the value of a field that corresponds to a named type.
fn resolve_field_value_for_named_type<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
    field: &q::Field,
    field_definition: &s::Field,
    type_name: &s::Name,
    argument_values: &HashMap<&q::Name, q::Value>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    // Try to resolve the type name into the actual type
    let named_type = sast::get_named_type(ctx.get_schema(), type_name)
        .ok_or_else(|| QueryExecutionError::NamedTypeError(type_name.to_string()))?;

    match named_type {
        // Let the resolver decide how the field (with the given object type)
        // is resolved into an entity based on the (potential) parent object
        s::TypeDefinition::Object(t) => ctx.resolve_object(
            object_value,
            field,
            field_definition,
            t.into(),
            argument_values,
        ),

        // Let the resolver decide how values in the resolved object value
        // map to values of GraphQL enums
        s::TypeDefinition::Enum(t) => match object_value {
            Some(q::Value::Object(o)) => ctx.resolve_enum_value(field, t, o.get(&field.name)),
            _ => Ok(q::Value::Null),
        },

        // Let the resolver decide how values in the resolved object value
        // map to values of GraphQL scalars
        s::TypeDefinition::Scalar(t) => match object_value {
            Some(q::Value::Object(o)) => {
                ctx.resolve_scalar_value(object_type, o, field, t, o.get(&field.name))
            }
            _ => Ok(q::Value::Null),
        },

        s::TypeDefinition::Interface(i) => ctx.resolve_object(
            object_value,
            field,
            field_definition,
            i.into(),
            argument_values,
        ),

        s::TypeDefinition::Union(_) => Err(QueryExecutionError::Unimplemented("unions".to_owned())),

        s::TypeDefinition::InputObject(_) => unreachable!("input objects are never resolved"),
    }
    .map_err(|e| vec![e])
}

/// Resolves the value of a field that corresponds to a list type.
fn resolve_field_value_for_list_type<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &s::ObjectType,
    object_value: &Option<q::Value>,
    field: &q::Field,
    field_definition: &s::Field,
    inner_type: &s::Type,
    argument_values: &HashMap<&q::Name, q::Value>,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    match inner_type {
        s::Type::NonNullType(inner_type) => resolve_field_value_for_list_type(
            ctx,
            object_type,
            object_value,
            field,
            field_definition,
            inner_type,
            argument_values,
        ),

        s::Type::NamedType(ref type_name) => {
            let named_type = sast::get_named_type(ctx.get_schema(), type_name)
                .ok_or_else(|| QueryExecutionError::NamedTypeError(type_name.to_string()))?;

            match named_type {
                // Let the resolver decide how the list field (with the given item object type)
                // is resolved into a entities based on the (potential) parent object
                s::TypeDefinition::Object(t) => ctx
                    .resolve_objects(
                        object_value,
                        &field.name,
                        field_definition,
                        t.into(),
                        argument_values,
                    )
                    .map_err(|e| vec![e]),

                // Let the resolver decide how values in the resolved object value
                // map to values of GraphQL enums
                s::TypeDefinition::Enum(t) => match object_value {
                    Some(q::Value::Object(o)) => {
                        ctx.resolve_enum_values(field, &t, o.get(&field.name))
                    }
                    _ => Ok(q::Value::Null),
                },

                // Let the resolver decide how values in the resolved object value
                // map to values of GraphQL scalars
                s::TypeDefinition::Scalar(t) => match object_value {
                    Some(q::Value::Object(o)) => {
                        ctx.resolve_scalar_values(field, &t, o.get(&field.name))
                    }
                    _ => Ok(q::Value::Null),
                },

                s::TypeDefinition::Interface(t) => ctx
                    .resolve_objects(
                        object_value,
                        &field.name,
                        field_definition,
                        t.into(),
                        argument_values,
                    )
                    .map_err(|e| vec![e]),

                s::TypeDefinition::Union(_) => Err(vec![QueryExecutionError::Unimplemented(
                    "unions".to_owned(),
                )]),

                s::TypeDefinition::InputObject(_) => {
                    unreachable!("input objects are never resolved")
                }
            }
        }

        // We don't support nested lists yet
        s::Type::ListType(_) => Err(vec![QueryExecutionError::Unimplemented(
            "nested list types".to_owned(),
        )]),
    }
}

/// Ensures that a value matches the expected return type.
fn complete_value<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    field: &'a q::Field,
    field_type: &'a s::Type,
    fields: Vec<&'a q::Field>,
    resolved_value: q::Value,
) -> Result<q::Value, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    match field_type {
        // Fail if the field type is non-null but the value is null
        s::Type::NonNullType(inner_type) => {
            return match complete_value(ctx.clone(), field, inner_type, fields, resolved_value)? {
                q::Value::Null => Err(vec![QueryExecutionError::NonNullError(
                    field.position,
                    field.name.to_string(),
                )]),

                v => Ok(v),
            };
        }

        // If the resolved value is null, return null
        _ if resolved_value == q::Value::Null => {
            return Ok(resolved_value);
        }

        // Complete list values
        s::Type::ListType(inner_type) => {
            return match resolved_value {
                // Complete list values individually
                q::Value::List(values) => {
                    let mut out = Vec::with_capacity(values.len());
                    for value in values.into_iter() {
                        out.push(complete_value(
                            ctx.clone(),
                            field,
                            inner_type,
                            fields.clone(),
                            value,
                        )?);
                    }
                    Ok(q::Value::List(out))
                }

                // Return field error if the resolved value for the list is not a list
                _ => Err(vec![QueryExecutionError::ListValueError(
                    field.position,
                    field.name.to_string(),
                )]),
            };
        }

        s::Type::NamedType(name) => {
            let named_type = sast::get_named_type(ctx.get_schema(), name).unwrap();

            match named_type {
                // Complete scalar values; we're assuming that the resolver has
                // already returned a valid value for the scalar type
                s::TypeDefinition::Scalar(_) => Ok(resolved_value),

                // Complete enum values; we're assuming that the resolver has
                // already returned a valid value for the enum type
                s::TypeDefinition::Enum(_) => Ok(resolved_value),

                // Complete object types recursively
                s::TypeDefinition::Object(object_type) => execute_selection_set(
                    ctx.clone(),
                    &merge_selection_sets(fields),
                    object_type,
                    &Some(resolved_value),
                ),

                // Resolve interface types using the resolved value and complete the value recursively
                s::TypeDefinition::Interface(_) => {
                    let object_type =
                        resolve_abstract_type(ctx.clone(), named_type, &resolved_value)?;

                    execute_selection_set(
                        ctx.clone(),
                        &merge_selection_sets(fields),
                        object_type,
                        &Some(resolved_value),
                    )
                }

                // Resolve union types using the resolved value and complete the value recursively
                s::TypeDefinition::Union(_) => {
                    let object_type =
                        resolve_abstract_type(ctx.clone(), named_type, &resolved_value)?;

                    execute_selection_set(
                        ctx.clone(),
                        &merge_selection_sets(fields),
                        object_type,
                        &Some(resolved_value),
                    )
                }

                s::TypeDefinition::InputObject(_) => {
                    unreachable!("input objects are never resolved")
                }
            }
        }
    }
}

/// Resolves an abstract type (interface, union) into an object type based on the given value.
fn resolve_abstract_type<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    abstract_type: &'a s::TypeDefinition,
    object_value: &q::Value,
) -> Result<&'a s::ObjectType, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    // Let the resolver handle the type resolution, return an error if the resolution
    // yields nothing
    ctx.resolver
        .resolve_abstract_type(
            // Can't get rid of this conditional because of lifetime headaches
            // "R1/R2 might not live long enough"
            if ctx.introspecting {
                ctx.introspection_schema
            } else {
                &ctx.schema.document
            },
            abstract_type,
            object_value,
        )
        .ok_or_else(|| {
            vec![QueryExecutionError::AbstractTypeError(
                sast::get_type_name(abstract_type).to_string(),
            )]
        })
}

/// Merges the selection sets of several fields into a single selection set.
fn merge_selection_sets(fields: Vec<&q::Field>) -> q::SelectionSet {
    let (span, items) = fields
        .iter()
        .fold((None, vec![]), |(span, mut items), field| {
            (
                // The overal span is the min/max spans of all merged selection sets
                match span {
                    None => Some(field.selection_set.span),
                    Some((start, end)) => Some((
                        cmp::min(start, field.selection_set.span.0),
                        cmp::max(end, field.selection_set.span.1),
                    )),
                },
                // The overall selection is the result of merging the selections of all fields
                {
                    items.extend_from_slice(field.selection_set.items.as_slice());
                    items
                },
            )
        });

    q::SelectionSet {
        span: span.unwrap(),
        items,
    }
}

/// Coerces argument values into GraphQL values.
pub fn coerce_argument_values<'a, R1, R2>(
    ctx: &ExecutionContext<'_, R1, R2>,
    object_type: &'a s::ObjectType,
    field: &q::Field,
) -> Result<HashMap<&'a q::Name, q::Value>, Vec<QueryExecutionError>>
where
    R1: Resolver,
    R2: Resolver,
{
    let mut coerced_values = HashMap::new();
    let mut errors = vec![];

    let resolver = |name: &Name| sast::get_named_type(ctx.get_schema(), name);

    for argument_def in sast::get_argument_definitions(object_type, &field.name)
        .into_iter()
        .flatten()
    {
        let value = qast::get_argument_value(&field.arguments, &argument_def.name).cloned();
        match coercion::coerce_input_value(value, &argument_def, &resolver, &ctx.variable_values) {
            Ok(Some(value)) => {
                coerced_values.insert(&argument_def.name, value);
            }
            Ok(None) => {}
            Err(e) => errors.push(e),
        }
    }

    if errors.is_empty() {
        Ok(coerced_values)
    } else {
        Err(errors)
    }
}

fn get_field_type<'a, R1, R2>(
    ctx: ExecutionContext<'a, R1, R2>,
    object_type: &'a s::ObjectType,
    name: &'a s::Name,
) -> Option<(&'a s::Field, bool)>
where
    R1: Resolver,
    R2: Resolver,
{
    // Resolve __schema and __Type using the introspection schema
    let introspection_query_type = sast::get_root_query_type(ctx.introspection_schema).unwrap();
    if let Some(ty) = sast::get_field_type(introspection_query_type, name) {
        return Some((ty, true));
    }

    sast::get_field_type(object_type, name).map(|t| (t, ctx.introspecting))
}

/// Coerces variable values for an operation.
pub fn coerce_variable_values(
    schema: &Schema,
    operation: &q::OperationDefinition,
    variables: &Option<QueryVariables>,
) -> Result<HashMap<q::Name, q::Value>, Vec<QueryExecutionError>> {
    let mut coerced_values = HashMap::new();
    let mut errors = vec![];

    for variable_def in qast::get_variable_definitions(operation)
        .into_iter()
        .flatten()
    {
        // Skip variable if it has an invalid type
        if !sast::is_input_type(&schema.document, &variable_def.var_type) {
            errors.push(QueryExecutionError::InvalidVariableTypeError(
                variable_def.position,
                variable_def.name.to_owned(),
            ));
            continue;
        }

        let value = variables
            .as_ref()
            .and_then(|vars| vars.get(&variable_def.name));

        let value = match value.or(variable_def.default_value.as_ref()) {
            // No variable value provided and no default for non-null type, fail
            None => {
                if sast::is_non_null_type(&variable_def.var_type) {
                    errors.push(QueryExecutionError::MissingVariableError(
                        variable_def.position,
                        variable_def.name.to_owned(),
                    ));
                };
                continue;
            }
            Some(value) => value,
        };

        // We have a variable value, attempt to coerce it to the value type
        // of the variable definition
        coerced_values.insert(
            variable_def.name.to_owned(),
            coerce_variable_value(schema, variable_def, &value)?,
        );
    }

    if errors.is_empty() {
        Ok(coerced_values)
    } else {
        Err(errors)
    }
}

fn coerce_variable_value(
    schema: &Schema,
    variable_def: &q::VariableDefinition,
    value: &q::Value,
) -> Result<q::Value, Vec<QueryExecutionError>> {
    use crate::values::coercion::coerce_value;
    use graphql_parser::schema::Name;

    let resolver = |name: &Name| sast::get_named_type(&schema.document, name);

    coerce_value(&value, &variable_def.var_type, &resolver, &HashMap::new()).ok_or_else(|| {
        vec![QueryExecutionError::InvalidArgumentError(
            variable_def.position,
            variable_def.name.to_owned(),
            value.clone(),
        )]
    })
}
