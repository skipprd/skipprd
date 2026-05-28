use serde::{Deserialize, Serialize};

pub trait CheckpointPayload: Serialize + for<'de> Deserialize<'de> {
    const VERSION: u32;
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct JsonCheckpoint<T> {
    pub version: u32,
    pub payload: T,
}

impl<T> JsonCheckpoint<T>
where
    T: CheckpointPayload,
{
    pub fn new(payload: T) -> Self {
        Self {
            version: T::VERSION,
            payload,
        }
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, serde_json::Error> {
        serde_json::from_slice(bytes)
    }

    pub fn validate_version(&self) -> Result<(), String> {
        if self.version != T::VERSION {
            return Err(format!(
                "checkpoint version mismatch: expected {} got {}",
                T::VERSION,
                self.version
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Ga4DayCheckpoint {
        pub last_date: String,
    }

    impl CheckpointPayload for Ga4DayCheckpoint {
        const VERSION: u32 = 1;
    }

    #[test]
    fn roundtrip_checkpoint_bytes() {
        let cp = JsonCheckpoint::new(Ga4DayCheckpoint {
            last_date: "2024-01-02".into(),
        });
        let bytes = cp.to_bytes().unwrap();
        let decoded: JsonCheckpoint<Ga4DayCheckpoint> = JsonCheckpoint::from_bytes(&bytes).unwrap();
        assert_eq!(decoded.payload.last_date, "2024-01-02");
        decoded.validate_version().unwrap();
    }

    #[test]
    fn checkpoint_version_mismatch_is_rejected() {
        let cp = JsonCheckpoint {
            version: 99,
            payload: Ga4DayCheckpoint {
                last_date: "2024-01-02".into(),
            },
        };
        let err = cp.validate_version().unwrap_err();
        assert!(err.contains("version mismatch"));
    }
}
