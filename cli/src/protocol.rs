use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Status,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorPayload {
    pub error: String,
}

impl Response {
    pub fn ok() -> Self {
        Response::Ok(OkPayload {
            status: "ok".to_string(),
            data: None,
        })
    }

    pub fn ok_with_data(data: serde_json::Value) -> Self {
        Response::Ok(OkPayload {
            status: "ok".to_string(),
            data: Some(data),
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
    fn status_request_wire_format() {
        let req = Request::Status;
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"method":"status"}"#);
        let parsed: Request = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, Request::Status);
    }

    #[test]
    fn ok_with_data_round_trip() {
        let data = serde_json::json!({"schema_version": 1, "status_code": "healthy"});
        let resp = Response::ok_with_data(data.clone());
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains(r#""status":"ok""#));
        assert!(json.contains(r#""data""#));
        let parsed: Response = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, resp);
        match parsed {
            Response::Ok(payload) => assert_eq!(payload.data, Some(data)),
            _ => panic!("expected Ok"),
        }
    }

    #[test]
    fn ok_without_data_backward_compat() {
        // Response::ok() produces no data field on the wire
        let resp = Response::ok();
        let json = serde_json::to_string(&resp).unwrap();
        assert_eq!(json, r#"{"status":"ok"}"#);
        // Can parse back from wire without data field
        let parsed: Response = serde_json::from_str(r#"{"status":"ok"}"#).unwrap();
        match parsed {
            Response::Ok(payload) => assert_eq!(payload.data, None),
            _ => panic!("expected Ok"),
        }
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
