use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Ping,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Response {
    Ok(OkPayload),
    Error(ErrorPayload),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OkPayload {
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub error: String,
}

impl Response {
    pub fn ok() -> Self {
        Response::Ok(OkPayload {
            status: "ok".to_string(),
        })
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Response::Error(ErrorPayload {
            error: msg.into(),
        })
    }

    pub fn into_result(self) -> Result<OkPayload, String> {
        match self {
            Response::Ok(payload) => Ok(payload),
            Response::Error(payload) => Err(payload.error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ping_request_round_trip() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Request::Ping);
    }

    #[test]
    fn ping_request_wire_format() {
        let req = Request::Ping;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"ping"}"#);
    }

    #[test]
    fn ok_response_round_trip() {
        let resp = Response::ok();
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn ok_response_wire_format() {
        let resp = Response::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"ok"}"#);
    }

    #[test]
    fn error_response_round_trip() {
        let resp = Response::err("something broke");
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
    }

    #[test]
    fn error_response_wire_format() {
        let resp = Response::err("invalid request");
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"error":"invalid request"}"#);
    }

    #[test]
    fn into_result_ok() {
        let resp = Response::ok();
        let result = resp.into_result();
        assert!(result.is_ok());
        assert_eq!(result.unwrap().status, "ok");
    }

    #[test]
    fn into_result_err() {
        let resp = Response::err("bad thing");
        let result = resp.into_result();
        assert_eq!(result.unwrap_err(), "bad thing");
    }

    #[test]
    fn unknown_method_fails_to_parse() {
        let json = r#"{"method":"bogus"}"#;
        let result = serde_json::from_str::<Request>(json);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_json_fails_to_parse() {
        let result = serde_json::from_str::<Request>("not json");
        assert!(result.is_err());
    }
}
