//! The AWS Query protocol, as a serde serializer.
//!
//! EC2 has no JSON dialect. Every call is a form-encoded `POST` whose
//! parameters are a flattened tree — `BlockDeviceMapping.1.Ebs.DeleteOnTermination`
//! — and whose answer is XML. Assembling those names with `format!` at each
//! call site is exactly the kind of structured document this codebase builds
//! with a serializer instead, so requests are declared as ordinary
//! `#[derive(Serialize)]` structs in [`super::ec2`] and this module is the
//! one place that knows how a name is composed.
//!
//! The mapping is the protocol's own:
//!
//! * a struct field becomes `Prefix.FieldName`, so `#[serde(rename = "…")]`
//!   is what pins the wire name;
//! * a sequence becomes `Prefix.1`, `Prefix.2`, … — **one-based**, which is
//!   the single detail most hand-rolled encoders get wrong;
//! * `None` is omitted entirely rather than sent empty, because EC2 reads an
//!   empty string as a value;
//! * a unit enum variant becomes its own name, which is how the closed sets
//!   (`stop`, `persistent`, `spot`) stay compile-checked.
//!
//! Anything outside that mapping — a map, a tuple, raw bytes — is an error
//! rather than a guess: the protocol has no encoding for them, and a
//! serializer that invented one would put a request on the wire that EC2
//! accepts and reads as something else.

use core::fmt;

use serde::{Serialize, ser};

/// Why a request could not be encoded.
///
/// Always a bug in this crate rather than something a caller did: the
/// request types are all declared here.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("an AWS query request could not be encoded: {0}")]
pub struct QueryError(String);

impl ser::Error for QueryError {
    fn custom<T: fmt::Display>(message: T) -> Self {
        Self(message.to_string())
    }
}

/// One request's parameters, in declaration order.
///
/// # Errors
///
/// Returns [`QueryError`] if the value uses a shape the Query protocol has
/// no encoding for.
pub fn to_pairs<T: Serialize + ?Sized>(value: &T) -> Result<Vec<(String, String)>, QueryError> {
    let mut serializer = QuerySerializer {
        prefix: String::new(),
        pairs: Vec::new(),
    };
    value.serialize(&mut serializer)?;
    Ok(serializer.pairs)
}

/// The serializer's state: where in the tree it is, and what it has emitted.
#[derive(Debug)]
struct QuerySerializer {
    prefix: String,
    pairs: Vec<(String, String)>,
}

impl QuerySerializer {
    /// Emits one leaf at the current prefix.
    ///
    /// A leaf at the root has no name to be filed under, which only happens
    /// if a caller serializes a bare scalar rather than a request.
    fn leaf(&mut self, value: String) -> Result<(), QueryError> {
        if self.prefix.is_empty() {
            return Err(QueryError(
                "a query request must be a struct, not a bare value".to_owned(),
            ));
        }
        self.pairs.push((self.prefix.clone(), value));
        Ok(())
    }

    /// Runs `body` with `segment` appended to the prefix, then restores it.
    fn nested<R>(
        &mut self,
        segment: &str,
        body: impl FnOnce(&mut Self) -> Result<R, QueryError>,
    ) -> Result<R, QueryError> {
        let restore = self.prefix.len();
        if !self.prefix.is_empty() {
            self.prefix.push('.');
        }
        self.prefix.push_str(segment);
        let result = body(self);
        self.prefix.truncate(restore);
        result
    }
}

/// The shapes the Query protocol cannot express.
fn unsupported(shape: &str) -> QueryError {
    QueryError(format!(
        "the AWS query protocol has no encoding for {shape}"
    ))
}

impl<'a> ser::Serializer for &'a mut QuerySerializer {
    type Ok = ();
    type Error = QueryError;
    type SerializeSeq = SeqSerializer<'a>;
    type SerializeTuple = ser::Impossible<(), QueryError>;
    type SerializeTupleStruct = ser::Impossible<(), QueryError>;
    type SerializeTupleVariant = ser::Impossible<(), QueryError>;
    type SerializeMap = ser::Impossible<(), QueryError>;
    type SerializeStruct = Self;
    type SerializeStructVariant = ser::Impossible<(), QueryError>;

    fn serialize_bool(self, value: bool) -> Result<(), QueryError> {
        self.leaf(if value { "true" } else { "false" }.to_owned())
    }

    fn serialize_i8(self, value: i8) -> Result<(), QueryError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i16(self, value: i16) -> Result<(), QueryError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i32(self, value: i32) -> Result<(), QueryError> {
        self.serialize_i64(i64::from(value))
    }

    fn serialize_i64(self, value: i64) -> Result<(), QueryError> {
        self.leaf(value.to_string())
    }

    fn serialize_u8(self, value: u8) -> Result<(), QueryError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u16(self, value: u16) -> Result<(), QueryError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u32(self, value: u32) -> Result<(), QueryError> {
        self.serialize_u64(u64::from(value))
    }

    fn serialize_u64(self, value: u64) -> Result<(), QueryError> {
        self.leaf(value.to_string())
    }

    fn serialize_f32(self, value: f32) -> Result<(), QueryError> {
        self.serialize_f64(f64::from(value))
    }

    fn serialize_f64(self, value: f64) -> Result<(), QueryError> {
        self.leaf(value.to_string())
    }

    fn serialize_char(self, value: char) -> Result<(), QueryError> {
        self.leaf(value.to_string())
    }

    fn serialize_str(self, value: &str) -> Result<(), QueryError> {
        self.leaf(value.to_owned())
    }

    fn serialize_bytes(self, _value: &[u8]) -> Result<(), QueryError> {
        Err(unsupported("raw bytes"))
    }

    /// An absent field is not sent at all: EC2 reads an empty value as a
    /// value.
    fn serialize_none(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), QueryError> {
        value.serialize(self)
    }

    fn serialize_unit(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), QueryError> {
        Ok(())
    }

    /// A closed set of wire tokens — `spot`, `persistent`, `stop` — which is
    /// how they stay compile-checked instead of being loose strings.
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _index: u32,
        variant: &'static str,
    ) -> Result<(), QueryError> {
        self.leaf(variant.to_owned())
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), QueryError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _value: &T,
    ) -> Result<(), QueryError> {
        Err(unsupported("a newtype enum variant"))
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<SeqSerializer<'a>, QueryError> {
        Err(unsupported("a sequence outside a struct field"))
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, QueryError> {
        Err(unsupported("a tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, QueryError> {
        Err(unsupported("a tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, QueryError> {
        Err(unsupported("a tuple enum variant"))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, QueryError> {
        Err(unsupported("a map"))
    }

    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, QueryError> {
        Ok(self)
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, QueryError> {
        Err(unsupported("a struct enum variant"))
    }
}

impl ser::SerializeStruct for &mut QuerySerializer {
    type Ok = ();
    type Error = QueryError;

    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        key: &'static str,
        value: &T,
    ) -> Result<(), QueryError> {
        self.nested(key, |serializer| {
            value.serialize(FieldSerializer(serializer))
        })
    }

    fn end(self) -> Result<(), QueryError> {
        Ok(())
    }
}

/// A field's value, which — unlike a bare value — may be a sequence.
///
/// The protocol indexes a list by the *field's* name, so a sequence only has
/// somewhere to go once a field name is on the prefix. Splitting it out this
/// way is what makes "a sequence at the root" an error rather than something
/// that silently emits `.1`.
struct FieldSerializer<'a>(&'a mut QuerySerializer);

/// One element of a list, filed under `Prefix.index`.
#[derive(Debug)]
pub struct SeqSerializer<'a> {
    serializer: &'a mut QuerySerializer,
    index: usize,
}

impl ser::SerializeSeq for SeqSerializer<'_> {
    type Ok = ();
    type Error = QueryError;

    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), QueryError> {
        // One-based, which is the protocol's own numbering.
        self.index += 1;
        let index = self.index.to_string();
        self.serializer
            .nested(&index, |serializer| value.serialize(&mut *serializer))
    }

    fn end(self) -> Result<(), QueryError> {
        Ok(())
    }
}

impl<'a> ser::Serializer for FieldSerializer<'a> {
    type Ok = ();
    type Error = QueryError;
    type SerializeSeq = SeqSerializer<'a>;
    type SerializeTuple = ser::Impossible<(), QueryError>;
    type SerializeTupleStruct = ser::Impossible<(), QueryError>;
    type SerializeTupleVariant = ser::Impossible<(), QueryError>;
    type SerializeMap = ser::Impossible<(), QueryError>;
    type SerializeStruct = &'a mut QuerySerializer;
    type SerializeStructVariant = ser::Impossible<(), QueryError>;

    fn serialize_seq(self, _len: Option<usize>) -> Result<SeqSerializer<'a>, QueryError> {
        Ok(SeqSerializer {
            serializer: self.0,
            index: 0,
        })
    }

    fn serialize_struct(
        self,
        name: &'static str,
        len: usize,
    ) -> Result<&'a mut QuerySerializer, QueryError> {
        ser::Serializer::serialize_struct(self.0, name, len)
    }

    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), QueryError> {
        value.serialize(self)
    }

    fn serialize_bool(self, value: bool) -> Result<(), QueryError> {
        ser::Serializer::serialize_bool(self.0, value)
    }

    fn serialize_i8(self, value: i8) -> Result<(), QueryError> {
        ser::Serializer::serialize_i8(self.0, value)
    }

    fn serialize_i16(self, value: i16) -> Result<(), QueryError> {
        ser::Serializer::serialize_i16(self.0, value)
    }

    fn serialize_i32(self, value: i32) -> Result<(), QueryError> {
        ser::Serializer::serialize_i32(self.0, value)
    }

    fn serialize_i64(self, value: i64) -> Result<(), QueryError> {
        ser::Serializer::serialize_i64(self.0, value)
    }

    fn serialize_u8(self, value: u8) -> Result<(), QueryError> {
        ser::Serializer::serialize_u8(self.0, value)
    }

    fn serialize_u16(self, value: u16) -> Result<(), QueryError> {
        ser::Serializer::serialize_u16(self.0, value)
    }

    fn serialize_u32(self, value: u32) -> Result<(), QueryError> {
        ser::Serializer::serialize_u32(self.0, value)
    }

    fn serialize_u64(self, value: u64) -> Result<(), QueryError> {
        ser::Serializer::serialize_u64(self.0, value)
    }

    fn serialize_f32(self, value: f32) -> Result<(), QueryError> {
        ser::Serializer::serialize_f32(self.0, value)
    }

    fn serialize_f64(self, value: f64) -> Result<(), QueryError> {
        ser::Serializer::serialize_f64(self.0, value)
    }

    fn serialize_char(self, value: char) -> Result<(), QueryError> {
        ser::Serializer::serialize_char(self.0, value)
    }

    fn serialize_str(self, value: &str) -> Result<(), QueryError> {
        ser::Serializer::serialize_str(self.0, value)
    }

    fn serialize_bytes(self, value: &[u8]) -> Result<(), QueryError> {
        ser::Serializer::serialize_bytes(self.0, value)
    }

    fn serialize_none(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn serialize_unit(self) -> Result<(), QueryError> {
        Ok(())
    }

    fn serialize_unit_struct(self, name: &'static str) -> Result<(), QueryError> {
        ser::Serializer::serialize_unit_struct(self.0, name)
    }

    fn serialize_unit_variant(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
    ) -> Result<(), QueryError> {
        ser::Serializer::serialize_unit_variant(self.0, name, index, variant)
    }

    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), QueryError> {
        value.serialize(self)
    }

    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        name: &'static str,
        index: u32,
        variant: &'static str,
        value: &T,
    ) -> Result<(), QueryError> {
        ser::Serializer::serialize_newtype_variant(self.0, name, index, variant, value)
    }

    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, QueryError> {
        Err(unsupported("a tuple"))
    }

    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, QueryError> {
        Err(unsupported("a tuple struct"))
    }

    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, QueryError> {
        Err(unsupported("a tuple enum variant"))
    }

    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, QueryError> {
        Err(unsupported("a map"))
    }

    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, QueryError> {
        Err(unsupported("a struct enum variant"))
    }
}

#[cfg(test)]
mod tests {
    use serde::Serialize;

    use super::to_pairs;

    #[derive(Serialize)]
    struct Ebs {
        #[serde(rename = "DeleteOnTermination")]
        delete_on_termination: bool,
        #[serde(rename = "VolumeSize")]
        volume_size: u32,
    }

    #[derive(Serialize)]
    struct Mapping {
        #[serde(rename = "DeviceName")]
        device_name: String,
        #[serde(rename = "Ebs")]
        ebs: Ebs,
    }

    #[derive(Serialize)]
    #[serde(rename_all = "lowercase")]
    enum MarketType {
        Spot,
    }

    #[derive(Serialize)]
    struct Request {
        #[serde(rename = "InstanceType")]
        instance_type: String,
        #[serde(rename = "MarketType")]
        market_type: Option<MarketType>,
        #[serde(rename = "SecurityGroupId")]
        security_group_id: Vec<String>,
        #[serde(rename = "BlockDeviceMapping")]
        block_device_mapping: Vec<Mapping>,
        #[serde(rename = "KeyName")]
        key_name: Option<String>,
    }

    fn request() -> Request {
        Request {
            instance_type: "t4g.small".to_owned(),
            market_type: Some(MarketType::Spot),
            security_group_id: vec!["sg-1".to_owned(), "sg-2".to_owned()],
            block_device_mapping: vec![Mapping {
                device_name: "/dev/sda1".to_owned(),
                ebs: Ebs {
                    delete_on_termination: false,
                    volume_size: 30,
                },
            }],
            key_name: None,
        }
    }

    #[test]
    fn a_nested_request_flattens_into_the_protocols_own_names() {
        assert_eq!(
            to_pairs(&request()).expect("encode"),
            vec![
                ("InstanceType".to_owned(), "t4g.small".to_owned()),
                ("MarketType".to_owned(), "spot".to_owned()),
                // Lists are one-based, which is the detail a hand-rolled
                // encoder gets wrong and EC2 answers with `MissingParameter`.
                ("SecurityGroupId.1".to_owned(), "sg-1".to_owned()),
                ("SecurityGroupId.2".to_owned(), "sg-2".to_owned()),
                (
                    "BlockDeviceMapping.1.DeviceName".to_owned(),
                    "/dev/sda1".to_owned()
                ),
                (
                    "BlockDeviceMapping.1.Ebs.DeleteOnTermination".to_owned(),
                    "false".to_owned()
                ),
                (
                    "BlockDeviceMapping.1.Ebs.VolumeSize".to_owned(),
                    "30".to_owned()
                ),
            ]
        );
    }

    #[test]
    fn an_absent_field_is_omitted_rather_than_sent_empty() {
        let pairs = to_pairs(&request()).expect("encode");
        assert!(
            !pairs.iter().any(|(name, _)| name == "KeyName"),
            "EC2 reads an empty value as a value"
        );
    }

    #[test]
    fn a_shape_the_protocol_cannot_express_is_an_error_rather_than_a_guess() {
        let map: std::collections::BTreeMap<&str, &str> = core::iter::once(("a", "b")).collect();
        to_pairs(&map).expect_err("the query protocol has no map encoding");
        to_pairs("a bare string").expect_err("a request is a struct");
    }
}
