use super::*;

pub(crate) fn stream_owner(request: Request<'_>, state: &ServerState) -> Result<String, JsonReply> {
    let accounts = query_values(request.target, "account_id");
    let owners = query_values(request.target, "owner");
    match (accounts.as_slice(), owners.as_slice()) {
        ([account], []) => match quota_owner_key(state, account) {
            Ok(owner) => Ok(owner),
            Err(_) => Err(JsonReply::error(404, "Account usage is unavailable", None)),
        },
        ([], [owner])
            if !owner.is_empty() && owner.len() <= 256 && !owner.chars().any(char::is_control) =>
        {
            Ok(owner.clone())
        }
        _ => Err(invalid()),
    }
}
