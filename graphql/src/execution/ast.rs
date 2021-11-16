use std::{collections::HashSet, ops::Deref};

use graph::{
    components::store::EntityType,
    data::graphql::{DocumentExt, ObjectOrInterface},
    prelude::{q, r, s, QueryExecutionError, Schema},
};
use graphql_parser::Pos;

use crate::schema::ast::ObjectCondition;

/// A selection set is a table that maps object types to the fields that
/// should be selected for objects of that type. The types are always
/// concrete object types, never interface or union types. When a
/// `SelectionSet` is constructed, fragments must already have been resolved
/// as it only allows using fields.
///
/// The set of types that a `SelectionSet` can accommodate must be set at
/// the time the `SelectionSet` is constructed. It is not possible to add
/// more types to it, but it is possible to add fields for all known types
/// or only some of them
#[derive(Debug, Clone, PartialEq)]
pub struct SelectionSet {
    // Map object types to the list of fields that should be selected for
    // them
    items: Vec<(String, Vec<Field>)>,
}

impl SelectionSet {
    /// Create a new `SelectionSet` that can handle the given types
    pub fn new(types: Vec<String>) -> Self {
        let items = types.into_iter().map(|name| (name, Vec::new())).collect();
        SelectionSet { items }
    }

    /// Create a new `SelectionSet` that can handle the same types as
    /// `other`, but ignore all fields from `other`
    pub fn empty_from(other: &SelectionSet) -> Self {
        let items = other
            .items
            .iter()
            .map(|(name, _)| (name.clone(), Vec::new()))
            .collect();
        SelectionSet { items }
    }

    /// Return `true` if this selection set does not select any fields for
    /// its types
    pub fn is_empty(&self) -> bool {
        self.items.iter().all(|(_, fields)| fields.is_empty())
    }

    /// If the selection set contains a single field across all its types,
    /// return it. Otherwise, return `None`
    pub fn single_field(&self) -> Option<&Field> {
        let mut iter = self.items.iter();
        let field = match iter.next() {
            Some((_, fields)) => {
                if fields.len() != 1 {
                    return None;
                } else {
                    &fields[0]
                }
            }
            None => return None,
        };
        for (_, fields) in iter {
            if fields.len() != 1 {
                return None;
            }
            if &fields[0] != field {
                return None;
            }
        }
        return Some(field);
    }

    /// Iterate over all types and the fields for those types
    pub fn fields(&self) -> impl Iterator<Item = (&str, impl Iterator<Item = &Field>)> {
        self.items
            .iter()
            .map(|(name, fields)| (name.as_str(), fields.iter()))
    }

    /// Iterate over all types and the fields that are not leaf fields, i.e.
    /// whose selection sets are not empty
    pub fn interior_fields(&self) -> impl Iterator<Item = (&str, impl Iterator<Item = &Field>)> {
        self.items.iter().map(|(name, fields)| {
            (
                name.as_str(),
                fields.iter().filter(|field| !field.is_leaf()),
            )
        })
    }

    /// Iterate over all fields for the given object type
    pub fn fields_for(&self, obj_type: &s::ObjectType) -> impl Iterator<Item = &Field> {
        let item = self
            .items
            .iter()
            .find(|(name, _)| name == &obj_type.name)
            .expect("there is an entry for the type");
        item.1.iter()
    }

    /// Append the field for all the sets' types
    pub fn push(&mut self, new_field: &Field) {
        for (_, fields) in &mut self.items {
            Self::merge_field(fields, new_field.clone());
        }
    }

    /// Append the fields for all the sets' types
    pub fn push_fields(&mut self, fields: Vec<&Field>) {
        for field in fields {
            self.push(field);
        }
    }

    /// Merge `self` with the fields from `other`, which must have the same,
    /// or a subset of, the types of `self`. The `directives` are added to
    /// `self`'s directives so that they take precedence over existing
    /// directives with the same name
    pub fn merge(&mut self, other: SelectionSet, directives: Vec<Directive>) {
        for (other_name, other_fields) in other.items {
            let item = self
                .items
                .iter_mut()
                .find(|(name, _)| &other_name == name)
                .expect("all possible types are already in items");
            for mut other_field in other_fields {
                other_field.prepend_directives(directives.clone());
                Self::merge_field(&mut item.1, other_field);
            }
        }
    }

    fn merge_field(fields: &mut Vec<Field>, new_field: Field) {
        match fields
            .iter_mut()
            .find(|field| field.response_key() == new_field.response_key())
        {
            Some(field) => {
                // TODO: check that _field and new_field are mergeable, in
                // particular that their name, directives and arguments are
                // compatible
                field.selection_set.merge(new_field.selection_set, vec![]);
            }
            None => fields.push(new_field),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub position: Pos,
    pub name: String,
    pub arguments: Vec<(String, r::Value)>,
}

impl Directive {
    /// Looks up the value of an argument of this directive
    pub fn argument_value(&self, name: &str) -> Option<&r::Value> {
        self.arguments
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    fn eval_if(&self) -> bool {
        match self.argument_value("if") {
            None => true,
            Some(r::Value::Boolean(b)) => *b,
            Some(_) => false,
        }
    }

    /// Return `true` if this directive says that we should not include the
    /// field it is attached to. That is the case if the directive is
    /// `include` and its `if` condition is `false`, or if it is `skip` and
    /// its `if` condition is `true`. In all other cases, return `false`
    pub fn skip(&self) -> bool {
        match self.name.as_str() {
            "include" => !self.eval_if(),
            "skip" => self.eval_if(),
            _ => false,
        }
    }
}

/// A field to execute as part of a query. When the field is constructed by
/// `Query::new`, variables are interpolated, and argument values have
/// already been coerced to the appropriate types for the field argument
#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub position: Pos,
    pub alias: Option<String>,
    pub name: String,
    pub arguments: Vec<(String, r::Value)>,
    pub directives: Vec<Directive>,
    pub selection_set: SelectionSet,
}

impl Field {
    /// Returns the response key of a field, which is either its name or its
    /// alias (if there is one).
    pub fn response_key(&self) -> &str {
        self.alias
            .as_ref()
            .map(Deref::deref)
            .unwrap_or(self.name.as_str())
    }

    /// Looks up the value of an argument for this field
    pub fn argument_value(&self, name: &str) -> Option<&r::Value> {
        self.arguments
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    fn prepend_directives(&mut self, mut directives: Vec<Directive>) {
        // TODO: check that the new directives don't conflict with existing
        // directives
        std::mem::swap(&mut self.directives, &mut directives);
        self.directives.extend(directives);
    }

    fn is_leaf(&self) -> bool {
        self.selection_set.is_empty()
    }
}

/// A set of object types, generated from resolving interfaces into the
/// object types that implement them, and possibly narrowing further when
/// expanding fragments with type conitions
#[derive(Debug, Clone, PartialEq)]
pub enum ObjectTypeSet {
    Any,
    Only(HashSet<String>),
}

impl ObjectTypeSet {
    pub fn convert(
        schema: &Schema,
        type_cond: Option<&q::TypeCondition>,
    ) -> Result<ObjectTypeSet, QueryExecutionError> {
        match type_cond {
            Some(q::TypeCondition::On(name)) => Self::from_name(schema, name),
            None => Ok(ObjectTypeSet::Any),
        }
    }

    pub fn from_name(schema: &Schema, name: &str) -> Result<ObjectTypeSet, QueryExecutionError> {
        let set = resolve_object_types(schema, name)?
            .into_iter()
            .map(|ty| ty.name().to_string())
            .collect();
        Ok(ObjectTypeSet::Only(set))
    }

    fn matches_name(&self, name: &str) -> bool {
        match self {
            ObjectTypeSet::Any => true,
            ObjectTypeSet::Only(set) => set.contains(name),
        }
    }

    pub fn intersect(self, other: &ObjectTypeSet) -> ObjectTypeSet {
        match self {
            ObjectTypeSet::Any => other.clone(),
            ObjectTypeSet::Only(set) => ObjectTypeSet::Only(
                set.into_iter()
                    .filter(|ty| other.matches_name(ty))
                    .collect(),
            ),
        }
    }

    /// Return a list of the object type names that are in this type set and
    /// are also implementations of `current_type`
    pub fn type_names(
        &self,
        schema: &Schema,
        current_type: ObjectOrInterface<'_>,
    ) -> Result<Vec<String>, QueryExecutionError> {
        Ok(resolve_object_types(schema, current_type.name())?
            .into_iter()
            .map(|obj| obj.name().to_string())
            .filter(|name| match self {
                ObjectTypeSet::Any => true,
                ObjectTypeSet::Only(set) => set.contains(name.as_str()),
            })
            .collect::<Vec<String>>())
    }
}

/// Look up the type `name` from the schema and resolve interfaces
/// and unions until we are left with a set of concrete object types
pub(crate) fn resolve_object_types<'a>(
    schema: &'a Schema,
    name: &str,
) -> Result<HashSet<ObjectCondition<'a>>, QueryExecutionError> {
    let mut set = HashSet::new();
    match schema
        .document
        .get_named_type(name)
        .ok_or_else(|| QueryExecutionError::AbstractTypeError(name.to_string()))?
    {
        s::TypeDefinition::Interface(intf) => {
            for obj_ty in &schema.types_for_interface()[&EntityType::new(intf.name.to_string())] {
                set.insert(obj_ty.into());
            }
        }
        s::TypeDefinition::Union(tys) => {
            for ty in &tys.types {
                set.extend(resolve_object_types(schema, ty)?)
            }
        }
        s::TypeDefinition::Object(ty) => {
            set.insert(ty.into());
        }
        s::TypeDefinition::Scalar(_)
        | s::TypeDefinition::Enum(_)
        | s::TypeDefinition::InputObject(_) => {
            return Err(QueryExecutionError::NamedTypeError(name.to_string()));
        }
    }
    Ok(set)
}
