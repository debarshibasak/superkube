use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::meta::{ObjectMeta, TypeMeta};
use super::service::ObjectReference;

/// Kubernetes-compatible Event resource
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    #[serde(flatten)]
    pub type_meta: TypeMeta,

    pub metadata: ObjectMeta,

    /// The object that this event is about
    pub involved_object: ObjectReference,

    /// Short, machine understandable string for the reason
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,

    /// Human-readable description of the event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,

    /// Component from which the event is generated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<EventSource>,

    /// Time when this Event was first observed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_timestamp: Option<DateTime<Utc>>,

    /// Time when this Event was last observed
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_timestamp: Option<DateTime<Utc>>,

    /// Number of times this event has occurred
    #[serde(skip_serializing_if = "Option::is_none")]
    pub count: Option<i32>,

    /// Type of this event (Normal, Warning)
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub event_type: Option<EventType>,

    /// Time when this Event was first observed (MicroTime)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub event_time: Option<DateTime<Utc>>,

    /// Action taken/failed regarding the Regarding object
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,

    /// Name of the controller that emitted this Event
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reporting_controller: Option<String>,

    /// ID of the controller instance
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reporting_instance: Option<String>,
}

/// Source of the event
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventSource {
    /// Component from which the event is generated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,

    /// Node name on which the event is generated
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

/// Type of event
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum EventType {
    Normal,
    Warning,
}

impl Default for EventType {
    fn default() -> Self {
        EventType::Normal
    }
}

/// List of events
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventList {
    #[serde(flatten)]
    pub type_meta: TypeMeta,

    pub metadata: super::meta::ListMeta,

    pub items: Vec<Event>,
}

impl Event {
    pub fn new(name: &str, namespace: &str) -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Event".to_string()),
            },
            metadata: ObjectMeta::new(name, namespace),
            involved_object: ObjectReference::default(),
            reason: None,
            message: None,
            source: None,
            first_timestamp: Some(Utc::now()),
            last_timestamp: Some(Utc::now()),
            count: Some(1),
            event_type: Some(EventType::Normal),
            event_time: None,
            action: None,
            reporting_controller: None,
            reporting_instance: None,
        }
    }

    /// Create a new event for a specific object
    pub fn for_object(
        name: &str,
        namespace: &str,
        involved_object: ObjectReference,
        reason: &str,
        message: &str,
        event_type: EventType,
    ) -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("Event".to_string()),
            },
            metadata: ObjectMeta::new(name, namespace),
            involved_object,
            reason: Some(reason.to_string()),
            message: Some(message.to_string()),
            source: None,
            first_timestamp: Some(Utc::now()),
            last_timestamp: Some(Utc::now()),
            count: Some(1),
            event_type: Some(event_type),
            event_time: Some(Utc::now()),
            action: None,
            reporting_controller: None,
            reporting_instance: None,
        }
    }
}

impl EventList {
    pub fn new(items: Vec<Event>) -> Self {
        Self {
            type_meta: TypeMeta {
                api_version: Some("v1".to_string()),
                kind: Some("EventList".to_string()),
            },
            metadata: super::meta::ListMeta::default(),
            items,
        }
    }
}
