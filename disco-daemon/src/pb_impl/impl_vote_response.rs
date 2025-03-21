use crate::protobuf;
use crate::raft_types::VoteResponse;

impl From<VoteResponse> for protobuf::VoteResponse {
  fn from(vote_resp: VoteResponse) -> Self {
    protobuf::VoteResponse {
      vote: Some(vote_resp.vote),
      vote_granted: vote_resp.vote_granted,
      last_log_id: vote_resp.last_log_id.map(|log_id| log_id.into()),
    }
  }
}

impl From<protobuf::VoteResponse> for VoteResponse {
  fn from(proto_vote_resp: protobuf::VoteResponse) -> Self {
    let vote = proto_vote_resp.vote.unwrap();
    let last_log_id = proto_vote_resp.last_log_id.map(|log_id| log_id.into());
    VoteResponse::new(vote, last_log_id, proto_vote_resp.vote_granted)
  }
}
