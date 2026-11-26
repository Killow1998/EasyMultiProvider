use serde::{
    Deserialize, Deserializer,
    de::{Error, MapAccess, SeqAccess, Visitor},
};
use serde_json::{Map, Value, map::Entry};
use std::fmt;

pub(crate) fn from_slice(bytes: &[u8]) -> serde_json::Result<Value> {
    serde_json::from_slice::<StrictValue>(bytes).map(|value| value.0)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Value, D::Error> {
    StrictValue::deserialize(deserializer).map(|value| value.0)
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_any(ValueVisitor).map(Self)
    }
}

struct ValueVisitor;

impl<'de> Visitor<'de> for ValueVisitor {
    type Value = Value;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("JSON without duplicate object keys")
    }

    fn visit_unit<E: Error>(self) -> Result<Value, E> {
        Ok(Value::Null)
    }

    fn visit_bool<E: Error>(self, value: bool) -> Result<Value, E> {
        Ok(Value::Bool(value))
    }

    fn visit_i64<E: Error>(self, value: i64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_u64<E: Error>(self, value: u64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_f64<E: Error>(self, value: f64) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_str<E: Error>(self, value: &str) -> Result<Value, E> {
        Ok(value.into())
    }

    fn visit_string<E: Error>(self, value: String) -> Result<Value, E> {
        Ok(Value::String(value))
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
        let mut values = Vec::new();
        while let Some(StrictValue(value)) = input.next_element()? {
            values.push(value);
        }
        Ok(Value::Array(values))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut input: A) -> Result<Value, A::Error> {
        let mut values = Map::new();
        while let Some(key) = input.next_key::<String>()? {
            match values.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert(input.next_value::<StrictValue>()?.0);
                }
                Entry::Occupied(_) => return Err(A::Error::custom("duplicate JSON object key")),
            }
        }
        Ok(Value::Object(values))
    }
}
