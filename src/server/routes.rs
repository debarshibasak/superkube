use axum::{
    routing::get,
    Router,
};
use std::sync::Arc;

use super::api;
use super::AppState;

pub fn api_routes() -> Router<Arc<AppState>> {
    Router::new()
        // Discovery & cluster-info
        .route("/version", get(api::version_info))
        .route("/healthz", get(|| async { "ok" }))
        .route("/livez", get(|| async { "ok" }))
        .route("/readyz", get(|| async { "ok" }))
        .route("/api", get(api::api_versions))
        .route("/api/v1", get(api::api_v1_resources))
        .route("/apis", get(api::api_groups))
        .route("/openapi/v2", get(api::openapi_v2))
        .route("/openapi/v3", get(api::openapi_v3))
        .route("/apis/apps/v1", get(api::apps_v1_resources))
        .route(
            "/apis/rbac.authorization.k8s.io/v1",
            get(api::rbac_v1_resources),
        )
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
        // StatefulSets (apps/v1)
        .route(
            "/apis/apps/v1/namespaces/:namespace/statefulsets",
            get(api::list_statefulsets).post(api::create_statefulset),
        )
        .route(
            "/apis/apps/v1/namespaces/:namespace/statefulsets/:name",
            get(api::get_statefulset)
                .put(api::update_statefulset)
                .delete(api::delete_statefulset),
        )
        .route("/apis/apps/v1/statefulsets", get(api::list_all_statefulsets))
        // DaemonSets (apps/v1)
        .route(
            "/apis/apps/v1/namespaces/:namespace/daemonsets",
            get(api::list_daemonsets).post(api::create_daemonset),
        )
        .route(
            "/apis/apps/v1/namespaces/:namespace/daemonsets/:name",
            get(api::get_daemonset)
                .put(api::update_daemonset)
                .delete(api::delete_daemonset),
        )
        .route("/apis/apps/v1/daemonsets", get(api::list_all_daemonsets))
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
        // ServiceAccounts
        .route(
            "/api/v1/namespaces/:namespace/serviceaccounts",
            get(api::list_service_accounts).post(api::create_service_account),
        )
        .route(
            "/api/v1/namespaces/:namespace/serviceaccounts/:name",
            get(api::get_service_account)
                .put(api::update_service_account)
                .delete(api::delete_service_account),
        )
        .route("/api/v1/serviceaccounts", get(api::list_all_service_accounts))
        // Secrets
        .route(
            "/api/v1/namespaces/:namespace/secrets",
            get(api::list_secrets).post(api::create_secret),
        )
        .route(
            "/api/v1/namespaces/:namespace/secrets/:name",
            get(api::get_secret)
                .put(api::update_secret)
                .delete(api::delete_secret),
        )
        .route("/api/v1/secrets", get(api::list_all_secrets))
        // ConfigMaps
        .route(
            "/api/v1/namespaces/:namespace/configmaps",
            get(api::list_config_maps).post(api::create_config_map),
        )
        .route(
            "/api/v1/namespaces/:namespace/configmaps/:name",
            get(api::get_config_map)
                .put(api::update_config_map)
                .delete(api::delete_config_map),
        )
        .route("/api/v1/configmaps", get(api::list_all_config_maps))
        // ClusterRoles
        .route(
            "/apis/rbac.authorization.k8s.io/v1/clusterroles",
            get(api::list_cluster_roles).post(api::create_cluster_role),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/clusterroles/:name",
            get(api::get_cluster_role)
                .put(api::update_cluster_role)
                .delete(api::delete_cluster_role),
        )
        // ReplicationControllers (stub — kubectl get all queries this)
        .route(
            "/api/v1/namespaces/:namespace/replicationcontrollers",
            get(api::list_replication_controllers),
        )
        .route(
            "/api/v1/replicationcontrollers",
            get(api::list_all_replication_controllers),
        )
        // ReplicaSets (stub — same reason)
        .route(
            "/apis/apps/v1/namespaces/:namespace/replicasets",
            get(api::list_replica_sets),
        )
        .route(
            "/apis/apps/v1/replicasets",
            get(api::list_all_replica_sets),
        )
        // ClusterRoleBindings
        .route(
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings",
            get(api::list_cluster_role_bindings).post(api::create_cluster_role_binding),
        )
        .route(
            "/apis/rbac.authorization.k8s.io/v1/clusterrolebindings/:name",
            get(api::get_cluster_role_binding)
                .put(api::update_cluster_role_binding)
                .delete(api::delete_cluster_role_binding),
        )
}
