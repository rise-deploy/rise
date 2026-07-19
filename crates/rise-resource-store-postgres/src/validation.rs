use rise_resource_api::{
    is_reserved_collection_name, is_reserved_resource_kind, validate_collection_name,
    validate_controller_id, validate_resource_group, validate_resource_kind,
    validate_resource_version, OrganizationSpec, OrganizationStatus, ResourceDefinitionSpec,
    ResourceDefinitionStatus, SpecValidator, ValidationError, RESOURCE_DEFINITION_KIND,
};

pub struct OrganizationValidator;

impl SpecValidator for OrganizationValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), ValidationError> {
        let parsed: OrganizationSpec = serde_json::from_value(spec.clone())
            .map_err(|e| ValidationError::new(format!("invalid Organization spec: {e}")))?;
        if parsed.display_name.trim().is_empty() {
            return Err(ValidationError::new(
                "Organization spec.displayName must not be empty",
            ));
        }
        Ok(())
    }

    fn validate_status(&self, status: &serde_json::Value) -> Result<(), ValidationError> {
        serde_json::from_value::<OrganizationStatus>(status.clone())
            .map_err(|e| ValidationError::new(format!("invalid Organization status: {e}")))?;
        Ok(())
    }
}

pub struct ResourceDefinitionValidator;

pub(crate) fn validate_new_resource_definition_identity(
    spec: &ResourceDefinitionSpec,
) -> Result<(), ValidationError> {
    // `ResourceDefinition` is the store's structural discriminator: its rows feed a projection
    // view and use a dedicated create/update/delete lifecycle. It therefore cannot be reused as
    // an ordinary custom kind in another API group.
    if spec.kind == RESOURCE_DEFINITION_KIND {
        return Err(ValidationError::new(format!(
            "kind '{RESOURCE_DEFINITION_KIND}' is reserved for the resource-definition system"
        )));
    }
    if is_reserved_collection_name(&spec.plural) {
        return Err(ValidationError::new(format!(
            "plural '{}' is a reserved collection name",
            spec.plural
        )));
    }
    if is_reserved_resource_kind(&spec.group, &spec.kind) {
        return Err(ValidationError::new(format!(
            "API group '{}' is reserved for built-in resources",
            spec.group
        )));
    }

    Ok(())
}

impl SpecValidator for ResourceDefinitionValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), ValidationError> {
        let parsed: ResourceDefinitionSpec = serde_json::from_value(spec.clone())
            .map_err(|e| ValidationError::new(format!("invalid ResourceDefinition spec: {e}")))?;

        validate_resource_group(&parsed.group)?;
        validate_resource_kind(&parsed.kind)?;
        validate_collection_name(&parsed.plural)?;

        // A ResourceDefinition is root-scoped when it declares no `parent`;
        // otherwise the parent reference must be well-formed.
        if let Some(parent) = &parsed.parent {
            let Some((group, version)) = parent.api_version.split_once('/') else {
                return Err(ValidationError::new(
                    "parent.apiVersion must be '<group>/<version>'",
                ));
            };
            validate_resource_group(group)?;
            validate_resource_version(version)?;
            validate_resource_kind(&parent.kind)?;
        }

        if parsed.versions.is_empty() {
            return Err(ValidationError::new(
                "ResourceDefinition must have at least one version",
            ));
        }
        let storage_count = parsed.versions.iter().filter(|v| v.storage).count();
        if storage_count != 1 {
            return Err(ValidationError::new(
                "ResourceDefinition must have exactly one storage version",
            ));
        }
        if !parsed.versions.iter().any(|v| v.served) {
            return Err(ValidationError::new(
                "ResourceDefinition must have at least one served version",
            ));
        }
        for v in &parsed.versions {
            validate_resource_version(&v.name)?;
        }

        for id in &parsed.allowed_status_controller_ids {
            validate_controller_id(id)?;
        }

        Ok(())
    }

    fn validate_status(&self, status: &serde_json::Value) -> Result<(), ValidationError> {
        serde_json::from_value::<ResourceDefinitionStatus>(status.clone())
            .map_err(|e| ValidationError::new(format!("invalid ResourceDefinition status: {e}")))?;
        Ok(())
    }
}

pub struct JsonSchemaValidator {
    validator: jsonschema::Validator,
}

impl JsonSchemaValidator {
    pub fn new(schema: serde_json::Value) -> Result<Self, ValidationError> {
        let validator = jsonschema::validator_for(&schema).map_err(|e| {
            ValidationError::new(format!("invalid JSON schema in ResourceDefinition: {e}"))
        })?;
        Ok(Self { validator })
    }
}

impl SpecValidator for JsonSchemaValidator {
    fn validate_spec(&self, spec: &serde_json::Value) -> Result<(), ValidationError> {
        let errors: Vec<String> = self
            .validator
            .iter_errors(spec)
            .map(|e| e.to_string())
            .collect();
        if !errors.is_empty() {
            return Err(ValidationError::new(format!(
                "spec validation failed: {}",
                errors.join("; ")
            )));
        }
        Ok(())
    }

    fn validate_status(&self, _status: &serde_json::Value) -> Result<(), ValidationError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resource_definition(group: &str, kind: &str, plural: &str) -> serde_json::Value {
        json!({
            "group": group,
            "kind": kind,
            "plural": plural,
            "versions": [{"name":"v1", "served":true, "storage":true}],
            "allowedStatusControllerIds": []
        })
    }

    #[test]
    fn rejects_every_kind_in_the_reserved_builtin_group() {
        for kind in [
            "User",
            "UserIdentity",
            "Role",
            "PlatformRoleBinding",
            "FutureBuiltIn",
        ] {
            let spec: ResourceDefinitionSpec =
                serde_json::from_value(resource_definition("rise.dev", kind, "customresources"))
                    .unwrap();
            let error = validate_new_resource_definition_identity(&spec).unwrap_err();
            assert!(error
                .to_string()
                .contains("API group 'rise.dev' is reserved"));
        }
    }

    #[test]
    fn allows_the_same_kind_name_in_another_group() {
        let spec =
            serde_json::from_value(resource_definition("other.example", "User", "customusers"))
                .unwrap();
        validate_new_resource_definition_identity(&spec).unwrap();
    }

    #[test]
    fn rejects_resource_definition_as_a_custom_kind_in_any_group() {
        let spec = serde_json::from_value(resource_definition(
            "other.example",
            RESOURCE_DEFINITION_KIND,
            "customdefinitions",
        ))
        .unwrap();
        let error = validate_new_resource_definition_identity(&spec).unwrap_err();
        assert!(error.to_string().contains("resource-definition system"));
    }
}
