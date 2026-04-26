use crate::Result;
use plist::Value;
use std::collections::BTreeMap;

pub(crate) struct KeyedArchive {
    pub(crate) objects: Vec<Value>,
    pub(crate) root: Value,
}

impl KeyedArchive {
    pub(crate) fn new(payload: Value) -> Result<Self> {
        let dict = payload
            .as_dictionary()
            .ok_or("Session.plist root is not a keyed archive dictionary")?;
        let objects = dict
            .get("$objects")
            .and_then(Value::as_array)
            .ok_or("Session.plist has no $objects")?
            .clone();
        let top = dict
            .get("$top")
            .and_then(Value::as_dictionary)
            .ok_or("Session.plist has no $top")?;
        let root_ref = top.values().next().ok_or("Session.plist $top is empty")?;
        let root = deref_from_objects(&objects, root_ref).clone();
        Ok(Self { objects, root })
    }

    pub(crate) fn deref<'a>(&'a self, value: &'a Value) -> &'a Value {
        deref_from_objects(&self.objects, value)
    }

    pub(crate) fn ns_array(&self, value: Option<&Value>) -> Vec<Value> {
        let Some(value) = value else {
            return Vec::new();
        };
        let Some(dict) = self.deref(value).as_dictionary() else {
            return Vec::new();
        };
        let Some(objects) = dict.get("NS.objects").and_then(Value::as_array) else {
            return Vec::new();
        };
        objects.iter().map(|value| self.deref(value).clone()).collect()
    }

    pub(crate) fn ns_dict(&self, value: Option<&Value>) -> BTreeMap<String, Value> {
        let Some(value) = value else {
            return BTreeMap::new();
        };
        let value = self.deref(value);
        let Some(dict) = value.as_dictionary() else {
            return BTreeMap::new();
        };
        let Some(keys) = dict.get("NS.keys").and_then(Value::as_array) else {
            return dict.iter().map(|(key, value)| (key.clone(), value.clone())).collect();
        };
        let Some(objects) = dict.get("NS.objects").and_then(Value::as_array) else {
            return BTreeMap::new();
        };
        keys.iter()
            .zip(objects.iter())
            .filter_map(|(key, value)| Some((self.as_text(key).ok()?, self.deref(value).clone())))
            .collect()
    }

    pub(crate) fn as_text(&self, value: &Value) -> Result<String> {
        let value = self.deref(value);
        match value {
            Value::String(text) => Ok(text.clone()),
            Value::Data(data) => Ok(String::from_utf8(data.clone())?),
            Value::Dictionary(dict) => {
                if let Some(Value::Data(data)) = dict.get("NS.bytes") {
                    Ok(String::from_utf8(data.clone())?)
                } else {
                    Err("unsupported text payload dictionary".into())
                }
            }
            _ => Err("unsupported text payload".into()),
        }
    }
}

fn deref_from_objects<'a>(objects: &'a [Value], value: &'a Value) -> &'a Value {
    if let Some(uid) = value.as_uid() {
        objects.get(uid.get() as usize).unwrap_or(value)
    } else {
        value
    }
}
