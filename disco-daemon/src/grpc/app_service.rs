use std::collections::BTreeMap;
use std::sync::Arc;

use tonic::Request;
use tonic::Response;
use tonic::Status;
use tracing::debug;

use crate::protobuf;
use crate::raft_types::*;
use crate::store::StateMachineStore;

/// External API service implementation providing key-value store operations.
/// This service handles client requests for getting and setting values in the distributed store.
///
/// # Responsibilities
/// - Handle key-value get operations
/// - Handle key-value set operations
/// - Ensure consistency through Raft consensus
///
/// # Protocol Safety
/// This service implements the client-facing API and should validate all inputs
/// before processing them through the Raft consensus protocol.
pub struct AppServiceImpl {
  /// The Raft node instance for consensus operations
  raft: Raft,
  /// The state machine store for direct reads
  /// The state machine's key-value store for direct reads
  state_machine_store: Arc<StateMachineStore>,
}

impl AppServiceImpl {
  /// Creates a new instance of the API service
  ///
  /// # Arguments
  /// * `raft` - The Raft node instance this service will use
  /// * `state_machine_store` - The state machine store for reading data
  pub fn new(raft: Raft, state_machine_store: Arc<StateMachineStore>) -> Self {
    AppServiceImpl {
      raft,
      state_machine_store,
    }
  }
}

#[tonic::async_trait]
impl protobuf::app_service_server::AppService for AppServiceImpl {
  /// Sets a value for a given key in the distributed store
  ///
  /// # Arguments
  /// * `request` - Contains the key and value to set
  ///
  /// # Returns
  /// * `Ok(Response)` - Success response after the value is set
  /// * `Err(Status)` - Error status if the set operation fails
  async fn set(
    &self,
    request: Request<protobuf::SetRequest>,
  ) -> Result<Response<protobuf::Response>, Status> {
    let req = request.into_inner();
    debug!("Processing set request for key: {}", req.key.clone());

    let res = self
      .raft
      .client_write(req.clone())
      .await
      .map_err(|e| Status::internal(format!("Failed to write to store: {}", e)))?;

    debug!("Successfully set value for key: {}", req.key);
    Ok(Response::new(res.data))
  }

  /// Gets a value for a given key from the distributed store
  ///
  /// # Arguments
  /// * `request` - Contains the key to retrieve
  ///
  /// # Returns
  /// * `Ok(Response)` - Success response containing the value
  /// * `Err(Status)` - Error status if the get operation fails
  async fn get(
    &self,
    request: Request<protobuf::GetRequest>,
  ) -> Result<Response<protobuf::Response>, Status> {
    let req = request.into_inner();
    debug!("Processing get request for key: {}", req.key);

    let sm = self
      .state_machine_store
      .state_machine
      .lock()
      .map_err(|e| Status::internal(format!("error getting lock on sm: {}", e)))?;
    let value = sm
      .data
      .get(&req.key)
      .ok_or_else(|| Status::internal(format!("Key not found: {}", req.key)))?
      .to_string();

    debug!("Successfully retrieved value for key: {}", req.key);
    Ok(Response::new(protobuf::Response { value: Some(value) }))
  }

  /// Initializes a new Raft cluster with the specified nodes
  ///
  /// # Arguments
  /// * `request` - Contains the initial set of nodes for the cluster
  ///
  /// # Returns
  /// * Success response with initialization details
  /// * Error if initialization fails
  async fn init(&self, request: Request<protobuf::InitRequest>) -> Result<Response<()>, Status> {
    debug!("Initializing Raft cluster");
    let req = request.into_inner();

    // Convert nodes into required format
    let nodes_map: BTreeMap<u64, protobuf::Node> = req
      .nodes
      .into_iter()
      .map(|node| {
        (
          node.node_id,
          protobuf::Node {
            rpc_addr: node.rpc_addr,
            node_id: node.node_id,
          },
        )
      })
      .collect();

    // Initialize the cluster
    let result = self
      .raft
      .initialize(nodes_map)
      .await
      .map_err(|e| Status::internal(format!("Failed to initialize cluster: {}", e)))?;

    debug!("Cluster initialization successful");
    Ok(Response::new(result))
  }

  /// Adds a learner node to the Raft cluster
  ///
  /// # Arguments
  /// * `request` - Contains the node information and blocking preference
  ///
  /// # Returns
  /// * Success response with learner addition details
  /// * Error if the operation fails
  async fn add_learner(
    &self,
    request: Request<protobuf::AddLearnerRequest>,
  ) -> Result<Response<protobuf::ClientWriteResponse>, Status> {
    let req = request.into_inner();

    let node = req
      .node
      .ok_or_else(|| Status::internal("Node information is required"))?;

    debug!("Adding learner node {}", node.node_id);

    let raft_node = Node {
      rpc_addr: node.rpc_addr.clone(),
      node_id: node.node_id,
    };

    let result = self
      .raft
      .add_learner(node.node_id, raft_node, true)
      .await
      .map_err(|e| Status::internal(format!("Failed to add learner node: {}", e)))?;

    debug!("Successfully added learner node {}", node.node_id);
    Ok(Response::new(result.into()))
  }

  /// Changes the membership of the Raft cluster
  ///
  /// # Arguments
  /// * `request` - Contains the new member set and retention policy
  ///
  /// # Returns
  /// * Success response with membership change details
  /// * Error if the operation fails
  async fn change_membership(
    &self,
    request: Request<protobuf::ChangeMembershipRequest>,
  ) -> Result<Response<protobuf::ClientWriteResponse>, Status> {
    let req = request.into_inner();

    debug!(
      "Changing membership. Members: {:?}, Retain: {}",
      req.members, req.retain
    );

    let result = self
      .raft
      .change_membership(req.members, req.retain)
      .await
      .map_err(|e| Status::internal(format!("Failed to change membership: {}", e)))?;

    debug!("Successfully changed cluster membership");
    Ok(Response::new(result.into()))
  }

  /// Retrieves metrics about the Raft node
  async fn metrics(
    &self,
    _request: Request<()>,
  ) -> Result<Response<protobuf::MetricsResponse>, Status> {
    debug!("Collecting metrics");
    let metrics = self.raft.metrics().borrow().clone();
    let resp = protobuf::MetricsResponse {
      membership: Some(metrics.membership_config.membership().clone().into()),
      other_metrics: metrics.to_string(),
    };
    Ok(Response::new(resp))
  }
}
