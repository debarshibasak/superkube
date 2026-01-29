use axum::{
    routing::get,
    Router,
};
use std::sync::Arc;

use super::api;
use super::AppState;

pub fn api_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Discovery endpoints
        .route("/api", get(api::api_versions))
        .route("/api/v1", get(api::api_v1_resources))
        .route("/apis", get(api::api_groups))
        .route("/apis/apps/v1", get(api::apps_v1_resources))
        // Pods
        .route(
            "/api/v1/namespaces/:namespace/pods",
            get(api::list_pods).post(api::create_pod),
        )
        .route(
            "/api/v1/namespaces/:namespace/pods/:name",
            get(api::get_pod).put(api::update_pod).delete(api::delete_pod),
        )
        .route(
            "/api/v1/namespaces/:namespace/pods/:name/status",
            get(api::get_pod_status).put(api::update_pod_status),
        )
        .route(
            "/api/v1/namespaces/:namespace/pods/:name/log",
            get(api::get_pod_logs),
        )
        .route(
            "/api/v1/namespaces/:namespace/pods/:name/exec",
            get(api::exec_pod),
        )
        .route(
            "/api/v1/namespaces/:namespace/pods/:name/portforward",
            get(api::port_forward_pod),
        )
        .route("/api/v1/pods", get(api::list_all_pods))
        // Services
        .route(
            "/api/v1/namespaces/:namespace/services",
            get(api::list_services).post(api::create_service),
        )
        .route(
            "/api/v1/namespaces/:namespace/services/:name",
            get(api::get_service)
                .put(api::update_service)
                .delete(api::delete_service),
        )
        .route("/api/v1/services", get(api::list_all_services))
        // Nodes
        .route("/api/v1/nodes", get(api::list_nodes).post(api::create_node))
        .route(
            "/api/v1/nodes/:name",
            get(api::get_node).put(api::update_node).delete(api::delete_node),
        )
        // Endpoints
        .route(
            "/api/v1/namespaces/:namespace/endpoints",
            get(api::list_endpoints),
        )
        .route(
            "/api/v1/namespaces/:namespace/endpoints/:name",
            get(api::get_endpoints),
        )
        // Deployments (apps/v1)
        .route(
            "/apis/apps/v1/namespaces/:namespace/deployments",
            get(api::list_deployments).post(api::create_deployment),
        )
        .route(
            "/apis/apps/v1/namespaces/:namespace/deployments/:name",
            get(api::get_deployment)
                .put(api::update_deployment)
                .delete(api::delete_deployment),
        )
        .route("/apis/apps/v1/deployments", get(api::list_all_deployments))
        // Namespaces
        .route(
            "/api/v1/namespaces",
            get(api::list_namespaces).post(api::create_namespace),
        )
        .route(
            "/api/v1/namespaces/:namespace",
            get(api::get_namespace)
                .put(api::update_namespace)
                .delete(api::delete_namespace),
        )
        // Events
        .route(
            "/api/v1/namespaces/:namespace/events",
            get(api::list_events).post(api::create_event),
        )
        .route(
            "/api/v1/namespaces/:namespace/events/:name",
            get(api::get_event).delete(api::delete_event),
        )
        .route("/api/v1/events", get(api::list_all_events))
}
