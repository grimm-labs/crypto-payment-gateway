//! Projects (docs/projects-spec.md §12, §13): `/api/admin/v1/projects`, and
//! the owner helpers every creation body and list filter goes through.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use db::{LegacyData, OwnerFilter, ProjectCounts, ProjectCreate, ProjectDelete, ProjectRow, ProjectUpdate};
use domain::{pick_color, validate_key_tag, validate_project_name, Owner, ProjectColor};
use jobs::AppState;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::admin::{deserialize_some, staff_actor};
use crate::middleware::{ClientIp, CurrentStaff};
use crate::ApiError;

/// A body's `project_id`: a project UUID or `"merchant"`.
pub(crate) fn parse_owner_field(value: &str) -> Result<Owner, ApiError> {
    Owner::parse_api(value).map_err(|err| ApiError::unprocessable(err.to_string()))
}

/// A body's `project_id` where only a project is allowed (keys, payments).
pub(crate) fn parse_project_id_field(value: Option<&str>) -> Result<String, ApiError> {
    let value = value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .ok_or_else(|| ApiError::unprocessable("project_id is required (a project UUID)".into()))?;
    match Owner::parse_api(value) {
        Ok(Owner::Project(id)) => Ok(id),
        _ => Err(ApiError::unprocessable("project_id must be a project UUID".into())),
    }
}

/// A list's `?project_id=<uuid>|merchant` filter (absent = every owner).
pub(crate) fn owner_filter_param(value: Option<&str>) -> Result<OwnerFilter, ApiError> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Ok(OwnerFilter::Any),
        Some(value) => match Owner::parse_api(value) {
            Ok(Owner::Project(id)) => Ok(OwnerFilter::Project(id)),
            Ok(Owner::Merchant) => Ok(OwnerFilter::Merchant),
            Err(_) => Err(ApiError::bad_request("project_id must be a project UUID or merchant")),
        },
    }
}

/// `409 Create a project first` while no project exists (spec §12): no
/// owned object can be created before the first project.
pub(crate) async fn require_any_project(state: &AppState) -> Result<(), ApiError> {
    match jobs::projects::require_any_project(state).await? {
        Ok(()) => Ok(()),
        Err(missing) => Err(ApiError::conflict(&missing.to_string())),
    }
}

fn project_json(project: &ProjectRow, counts: &ProjectCounts) -> Value {
    json!({
        "id": project.id,
        "name": project.name,
        "color": project.color,
        "key_tag": project.key_tag,
        "created_at": project.created_at.to_rfc3339(),
        "updated_at": project.updated_at.to_rfc3339(),
        "counts": counts,
    })
}

/// `GET /projects` (any staff): every project with its counts, plus the
/// legacy rows still unassigned while no project exists (`null` after).
pub(crate) async fn list_projects(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let projects = state.db.list_projects().await?;
    let unassigned = if projects.is_empty() {
        Some(state.db.unassigned_counts().await?)
    } else {
        None
    };
    jobs::projects::note_projects_exist(!projects.is_empty());
    Ok(Json(json!({
        "projects": projects.iter().map(|row| project_json(&row.project, &row.counts)).collect::<Vec<_>>(),
        "unassigned": unassigned,
    })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct CreateProjectRequest {
    name: String,
    /// Palette colour; absent = the first colour no project uses yet.
    color: Option<String>,
    /// Tag of the project's keys; absent or blank = the global tag.
    key_tag: Option<String>,
    /// First project only: attach the legacy rows (default) or release them
    /// to the merchant (spec §13).
    attach_existing: Option<bool>,
}

/// `POST /projects` (ADMIN, no TOTP — audited): `201`.
pub(crate) async fn create_project(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Json(body): Json<CreateProjectRequest>,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let name = validate_project_name(&body.name).map_err(|err| ApiError::bad_request(&err.to_string()))?;
    let key_tag = validate_key_tag(body.key_tag.as_deref().unwrap_or_default())
        .map_err(|err| ApiError::bad_request(&err.to_string()))?;
    let color = match body.color.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
        Some(color) => ProjectColor::parse(color).map_err(|err| ApiError::bad_request(&err.to_string()))?,
        None => pick_color(&state.db.project_colors().await?),
    };
    let legacy = body.attach_existing.map(|attach| if attach { LegacyData::Attach } else { LegacyData::Release });
    let (project, outcome) = match state
        .db
        .create_project(
            &db::NewProject {
                name: &name,
                color,
                key_tag: key_tag.as_deref(),
                created_by_id: &current.id,
            },
            legacy,
        )
        .await?
    {
        ProjectCreate::Created(created) => (created.project, created.legacy),
        ProjectCreate::NameTaken => return Err(ApiError::conflict("A project with this name already exists")),
        ProjectCreate::KeyTagTaken => return Err(ApiError::conflict("Another project already uses this key tag")),
        ProjectCreate::NotFirst => {
            return Err(ApiError::conflict(
                "attach_existing is only accepted when creating the first project",
            ))
        }
    };
    jobs::projects::note_projects_exist(true);
    let actor = staff_actor(&current, &client_ip);
    jobs::audit::record(
        &state,
        &actor,
        "project.created",
        Some(("project", &project.id)),
        json!({
            "name": project.name,
            "color": project.color,
            "key_tag": project.key_tag,
            "legacy": outcome,
        }),
    )
    .await;
    if let Some(outcome) = &outcome {
        for key_id in &outcome.api_keys_revoked {
            jobs::audit::record(
                &state,
                &actor,
                "api_key.revoked",
                Some(("api_key", key_id)),
                json!({ "reason": "no_project" }),
            )
            .await;
        }
    }
    let counts = state
        .db
        .list_projects()
        .await?
        .into_iter()
        .find(|row| row.project.id == project.id)
        .map(|row| row.counts)
        .unwrap_or_default();
    Ok((StatusCode::CREATED, Json(project_json(&project, &counts))))
}

#[derive(Debug, Deserialize)]
pub(crate) struct UpdateProjectRequest {
    name: Option<String>,
    color: Option<String>,
    /// Absent = untouched; `null` or blank clears the tag.
    #[serde(default, deserialize_with = "deserialize_some")]
    key_tag: Option<Option<String>>,
}

/// `PATCH /projects/{id}` (ADMIN): name, colour, key tag.
pub(crate) async fn update_project(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
    Json(body): Json<UpdateProjectRequest>,
) -> Result<Json<Value>, ApiError> {
    if body.name.is_none() && body.color.is_none() && body.key_tag.is_none() {
        return Err(ApiError::bad_request("Nothing to update"));
    }
    let name = body
        .name
        .as_deref()
        .map(validate_project_name)
        .transpose()
        .map_err(|err| ApiError::bad_request(&err.to_string()))?;
    let color = body
        .color
        .as_deref()
        .map(ProjectColor::parse)
        .transpose()
        .map_err(|err| ApiError::bad_request(&err.to_string()))?;
    let key_tag = body
        .key_tag
        .as_ref()
        .map(|tag| validate_key_tag(tag.as_deref().unwrap_or_default()))
        .transpose()
        .map_err(|err| ApiError::bad_request(&err.to_string()))?;
    let project = match state
        .db
        .update_project(&id, name.as_deref(), color, key_tag.as_ref().map(|tag| tag.as_deref()))
        .await?
    {
        ProjectUpdate::Updated(project) => project,
        ProjectUpdate::NotFound => return Err(ApiError::not_found(format!("Project not found: {id}"))),
        ProjectUpdate::NameTaken => return Err(ApiError::conflict("A project with this name already exists")),
        ProjectUpdate::KeyTagTaken => return Err(ApiError::conflict("Another project already uses this key tag")),
    };
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "project.updated",
        Some(("project", &project.id)),
        json!({
            "name": name,
            "color": color.map(|c| c.as_str()),
            "key_tag": key_tag,
        }),
    )
    .await;
    let counts = state
        .db
        .list_projects()
        .await?
        .into_iter()
        .find(|row| row.project.id == project.id)
        .map(|row| row.counts)
        .unwrap_or_default();
    Ok(Json(project_json(&project, &counts)))
}

/// `DELETE /projects/{id}` (ADMIN): only an empty project, never the last.
pub(crate) async fn delete_project(
    State(state): State<AppState>,
    Extension(current): Extension<CurrentStaff>,
    client_ip: ClientIp,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    match state.db.delete_project(&id).await? {
        ProjectDelete::Deleted => {}
        ProjectDelete::NotFound => return Err(ApiError::not_found(format!("Project not found: {id}"))),
        ProjectDelete::InUse => {
            return Err(ApiError::conflict(
                "This project still owns keys, payments, withdrawals, transfers, endpoints or address-book entries",
            ))
        }
        ProjectDelete::LastProject => return Err(ApiError::conflict("The last project cannot be deleted")),
    }
    jobs::audit::record(
        &state,
        &staff_actor(&current, &client_ip),
        "project.deleted",
        Some(("project", &id)),
        json!({}),
    )
    .await;
    Ok(Json(json!({ "status": "deleted", "id": id })))
}

#[derive(Debug, Deserialize)]
pub(crate) struct OwnerListQuery {
    /// `<uuid>`, or `merchant` where the merchant can own rows.
    pub(crate) project_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_fields_and_filters_parse() {
        let id = "6f1c7a8e-2f43-4d55-9a8b-1c2d3e4f5a6b";
        assert_eq!(parse_owner_field(id).unwrap(), Owner::Project(id.into()));
        assert_eq!(parse_owner_field("merchant").unwrap(), Owner::Merchant);
        assert_eq!(parse_owner_field("shop").unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(parse_project_id_field(Some(id)).unwrap(), id);
        assert_eq!(parse_project_id_field(None).unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(parse_project_id_field(Some("")).unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);
        // Keys and payments are never the merchant's.
        assert_eq!(parse_project_id_field(Some("merchant")).unwrap_err().status, StatusCode::UNPROCESSABLE_ENTITY);

        assert_eq!(owner_filter_param(None).unwrap(), OwnerFilter::Any);
        assert_eq!(owner_filter_param(Some(" ")).unwrap(), OwnerFilter::Any);
        assert_eq!(owner_filter_param(Some(id)).unwrap(), OwnerFilter::Project(id.into()));
        assert_eq!(owner_filter_param(Some("merchant")).unwrap(), OwnerFilter::Merchant);
        assert_eq!(owner_filter_param(Some("x")).unwrap_err().status, StatusCode::BAD_REQUEST);
    }
}
