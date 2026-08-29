//! Wire encoding: JSON text frames over the WebSocket. (Compact binary
//! can replace this behind the same two functions later.)

use serde::{de::DeserializeOwned, Serialize};

pub fn encode<T: Serialize>(msg: &T) -> String {
    serde_json::to_string(msg).expect("protocol types always serialize")
}

pub fn decode<T: DeserializeOwned>(s: &str) -> Result<T, serde_json::Error> {
    serde_json::from_str(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::{ClientMsg, ServerMsg};

    #[test]
    fn round_trips() {
        let c = ClientMsg::Hello {
            proto_version: crate::PROTOCOL_VERSION,
            name: "ace".into(),
            password: "pw".into(),
        };
        let c2: ClientMsg = decode(&encode(&c)).unwrap();
        assert!(matches!(c2, ClientMsg::Hello { proto_version: 1, .. }));

        let s = ServerMsg::GameCreated { code: "AB12".into() };
        let s2: ServerMsg = decode(&encode(&s)).unwrap();
        assert!(matches!(s2, ServerMsg::GameCreated { code } if code == "AB12"));
    }
}
