//! The JSON messages of the batch API.
use std::collections::BTreeMap;

use crate::Operation;

/// The media type of batch API requests and responses.
pub const MEDIA_TYPE: &str = "application/vnd.git-lfs+json";

/// A request to `<endpoint>/objects/batch`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Request {
    /// What to do with the objects.
    pub operation: Operation,
    /// The transfer adapters the client supports.
    pub transfers: Vec<String>,
    /// The reference the objects belong to.
    #[serde(rename = "ref", skip_serializing_if = "Option::is_none")]
    pub reference: Option<Ref>,
    /// The objects to transfer.
    pub objects: Vec<ObjectSpec>,
    /// The hash algorithm of the object ids.
    pub hash_algo: String,
}

/// A reference in a batch [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Ref {
    /// The full name of the reference, like `refs/heads/main`.
    pub name: String,
}

/// An object in a batch [`Request`], and the body of a verify request.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct ObjectSpec {
    /// The id of the object as hexadecimal string.
    pub oid: String,
    /// The size of the object in bytes.
    pub size: u64,
}

/// The successful answer to a batch [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Response {
    /// The transfer adapter the server chose, `basic` if absent.
    pub transfer: Option<String>,
    /// The objects of the request, with what to do with each of them.
    pub objects: Vec<ObjectResponse>,
}

/// What to do with one object of a batch [`Request`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ObjectResponse {
    /// The id of the object as hexadecimal string.
    pub oid: String,
    /// The size of the object in bytes.
    pub size: u64,
    /// The actions to perform, absent for uploads of objects the server already has.
    pub actions: Option<Actions>,
    /// Why the object can't be transferred.
    pub error: Option<ObjectError>,
}

/// The actions to perform for an object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Actions {
    /// How to download the object.
    pub download: Option<Action>,
    /// How to upload the object.
    pub upload: Option<Action>,
    /// How to confirm an upload.
    pub verify: Option<Action>,
}

/// A request to make for one object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Action {
    /// The URL to send the request to.
    pub href: String,
    /// The headers to send with the request.
    #[serde(default)]
    pub header: BTreeMap<String, String>,
    /// The amount of seconds after which the action expires, taking precedence over `expires_at`.
    pub expires_in: Option<i64>,
    /// The time at which the action expires in RFC 3339 format.
    pub expires_at: Option<String>,
}

/// An error for one object.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ObjectError {
    /// An HTTP status code describing the error, like 404 if the object doesn't exist.
    pub code: u16,
    /// A description of the error.
    pub message: String,
}

/// The body of error responses of the batch API.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct ErrorResponse {
    /// A description of the error.
    pub message: String,
    /// A URL with more information.
    pub documentation_url: Option<String>,
    /// An id of the request for support.
    pub request_id: Option<String>,
}
