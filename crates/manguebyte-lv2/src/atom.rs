//! Reading atoms from bytes, without trusting them.
//!
//! The host hands the plugin a sequence of events in its own memory. The
//! wrapper turns that memory into one byte slice, the size the sequence's
//! header gives, and everything here reads from the slice with bounds
//! checks. A size field that points past the end stops the reading; nothing
//! is ever read outside the slice, and nothing allocates.

/// Rounds `size` up to the 8-byte boundary the atom specification pads
/// every event and property to.
fn padded(size: usize) -> Option<usize> {
    size.checked_add(7).map(|size| size & !7)
}

fn u32_at(bytes: &[u8], at: usize) -> Option<u32> {
    let field = bytes.get(at..at.checked_add(4)?)?;
    Some(u32::from_ne_bytes(field.try_into().ok()?))
}

fn i64_at(bytes: &[u8], at: usize) -> Option<i64> {
    let field = bytes.get(at..at.checked_add(8)?)?;
    Some(i64::from_ne_bytes(field.try_into().ok()?))
}

/// The atom types a number can arrive as.
#[derive(Debug, Clone, Copy)]
pub(crate) struct NumberTypes {
    pub int: u32,
    pub long: u32,
    pub float: u32,
    pub double: u32,
}

/// Reads an atom body of one of the number types as a float. `None` for
/// any other type, a body of the wrong size, or a value that is not finite.
pub(crate) fn number(types: NumberTypes, type_: u32, body: &[u8]) -> Option<f64> {
    let value = if type_ == types.int {
        f64::from(i32::from_ne_bytes(body.get(..4)?.try_into().ok()?))
    } else if type_ == types.long {
        i64::from_ne_bytes(body.get(..8)?.try_into().ok()?) as f64
    } else if type_ == types.float {
        f64::from(f32::from_ne_bytes(body.get(..4)?.try_into().ok()?))
    } else if type_ == types.double {
        f64::from_ne_bytes(body.get(..8)?.try_into().ok()?)
    } else {
        return None;
    };
    value.is_finite().then_some(value)
}

/// A sequence read from its bytes: the 8-byte atom header, the 8-byte
/// sequence body, then the events.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Sequence<'a> {
    /// The URID of the time stamps' unit, zero for frames.
    pub unit: u32,
    events: &'a [u8],
}

impl<'a> Sequence<'a> {
    /// Reads `bytes` as a sequence of type `sequence_type`. `None` if the
    /// header names another type or the bytes are too short for it.
    pub fn read(bytes: &'a [u8], sequence_type: u32) -> Option<Self> {
        let size = u32_at(bytes, 0)? as usize;
        if u32_at(bytes, 4)? != sequence_type || size < 8 {
            return None;
        }
        let end = size.checked_add(8)?.min(bytes.len());
        Some(Self {
            unit: u32_at(bytes, 8)?,
            events: bytes.get(16..end)?,
        })
    }

    /// The events, in the order they are stored.
    pub fn events(&self) -> Events<'a> {
        Events {
            bytes: self.events,
            at: 0,
        }
    }
}

/// One event of a sequence.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AtomEvent<'a> {
    /// The time stamp, in the sequence's unit.
    pub time: i64,
    /// The URID of the event's type.
    pub type_: u32,
    /// The event's body, without padding.
    pub body: &'a [u8],
}

/// The events of a [`Sequence`]. Stops at the first event that does not
/// fit in what is left.
#[derive(Debug, Clone)]
pub(crate) struct Events<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Events<'a> {
    type Item = AtomEvent<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let time = i64_at(self.bytes, self.at)?;
        let size = u32_at(self.bytes, self.at + 8)? as usize;
        let type_ = u32_at(self.bytes, self.at + 12)?;
        let start = self.at + 16;
        let Some(body) = start
            .checked_add(size)
            .and_then(|end| self.bytes.get(start..end))
        else {
            // Claims more than there is: nothing after it can be trusted.
            self.at = self.bytes.len();
            return None;
        };
        self.at = padded(size)
            .and_then(|size| start.checked_add(size))
            .unwrap_or(usize::MAX);
        Some(AtomEvent { time, type_, body })
    }
}

/// The body of an object: its id, its type, then its properties.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Object<'a> {
    /// The URID of the object's type.
    pub otype: u32,
    properties: &'a [u8],
}

impl<'a> Object<'a> {
    /// Reads the body of an object atom.
    pub fn read(body: &'a [u8]) -> Option<Self> {
        Some(Self {
            otype: u32_at(body, 4)?,
            properties: body.get(8..)?,
        })
    }

    /// The properties, in the order they are stored.
    pub fn properties(&self) -> Properties<'a> {
        Properties {
            bytes: self.properties,
            at: 0,
        }
    }
}

/// One property of an object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Property<'a> {
    /// The URID of the property.
    pub key: u32,
    /// The URID of the value's type.
    pub type_: u32,
    /// The value's body, without padding.
    pub body: &'a [u8],
}

/// The properties of an [`Object`]. Stops at the first property that does
/// not fit in what is left.
#[derive(Debug, Clone)]
pub(crate) struct Properties<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Iterator for Properties<'a> {
    type Item = Property<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let key = u32_at(self.bytes, self.at)?;
        let size = u32_at(self.bytes, self.at + 8)? as usize;
        let type_ = u32_at(self.bytes, self.at + 12)?;
        let start = self.at + 16;
        let Some(body) = start
            .checked_add(size)
            .and_then(|end| self.bytes.get(start..end))
        else {
            self.at = self.bytes.len();
            return None;
        };
        self.at = padded(size)
            .and_then(|size| start.checked_add(size))
            .unwrap_or(usize::MAX);
        Some(Property { key, type_, body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEQUENCE: u32 = 7;

    fn words(values: &[u32]) -> Vec<u8> {
        values
            .iter()
            .flat_map(|value| value.to_ne_bytes())
            .collect()
    }

    /// A sequence header claiming `size` bytes of body, with unit 0.
    fn header(size: u32) -> Vec<u8> {
        words(&[size, SEQUENCE, 0, 0])
    }

    fn event(time: i64, type_: u32, body: &[u8]) -> Vec<u8> {
        let mut bytes = time.to_ne_bytes().to_vec();
        bytes.extend(words(&[body.len() as u32, type_]));
        bytes.extend(body);
        bytes.resize(bytes.len().next_multiple_of(8), 0);
        bytes
    }

    #[test]
    fn events_are_read_with_their_padding() {
        let mut events = event(3, 11, &[0x90, 60, 100]);
        events.extend(event(9, 12, &[1, 2, 3, 4, 5, 6, 7, 8, 9]));
        let mut bytes = header(8 + events.len() as u32);
        bytes.extend(&events);

        let sequence = Sequence::read(&bytes, SEQUENCE).unwrap();
        let read: Vec<_> = sequence.events().collect();
        assert_eq!(
            read,
            [
                AtomEvent {
                    time: 3,
                    type_: 11,
                    body: &[0x90, 60, 100]
                },
                AtomEvent {
                    time: 9,
                    type_: 12,
                    body: &[1, 2, 3, 4, 5, 6, 7, 8, 9]
                },
            ]
        );
    }

    #[test]
    fn a_wrong_type_or_a_short_header_is_not_a_sequence() {
        assert!(Sequence::read(&words(&[8, SEQUENCE + 1, 0, 0]), SEQUENCE).is_none());
        assert!(Sequence::read(&words(&[8, SEQUENCE, 0]), SEQUENCE).is_none());
        assert!(Sequence::read(&words(&[4, SEQUENCE, 0, 0]), SEQUENCE).is_none());
        assert!(Sequence::read(&[], SEQUENCE).is_none());
    }

    /// Every way a size can lie stops the reading inside the bytes.
    #[test]
    fn sizes_that_lie_stop_the_reading() {
        let good = event(1, 11, &[0x80, 60, 0]);
        // After the lying header there are 32 bytes: 8 of its own and the
        // 24 of the good event. Any claim beyond them is a lie.
        for lie in [u32::MAX, u32::MAX - 6, 1 << 20, 33] {
            let mut bad = 2i64.to_ne_bytes().to_vec();
            bad.extend(words(&[lie, 11, 0, 0]));
            let mut events = good.clone();
            events.extend(&bad);
            events.extend(&good);
            let mut bytes = header(8 + events.len() as u32);
            bytes.extend(&events);
            let read: Vec<_> = Sequence::read(&bytes, SEQUENCE).unwrap().events().collect();
            assert_eq!(read.len(), 1, "lie {lie}");
        }

        // A header claiming more than the slice holds reads what is there.
        let mut bytes = header(1 << 30);
        bytes.extend(&good);
        let read: Vec<_> = Sequence::read(&bytes, SEQUENCE).unwrap().events().collect();
        assert_eq!(read.len(), 1);

        // Trailing bytes too few for an event header are ignored.
        let mut bytes = header(8 + good.len() as u32 + 5);
        bytes.extend(&good);
        bytes.extend([0; 5]);
        let read: Vec<_> = Sequence::read(&bytes, SEQUENCE).unwrap().events().collect();
        assert_eq!(read.len(), 1);
    }

    #[test]
    fn objects_and_numbers_are_read_and_checked() {
        let types = NumberTypes {
            int: 1,
            long: 2,
            float: 3,
            double: 4,
        };
        let mut body = words(&[0, 99]);
        body.extend(words(&[50, 0, 4, 3]));
        body.extend(120.0f32.to_ne_bytes());
        body.extend([0; 4]);
        body.extend(words(&[51, 0, 8, 2]));
        body.extend(1_000i64.to_ne_bytes());
        // Cut short: claims 8 bytes, has 4.
        body.extend(words(&[52, 0, 8, 4]));
        body.extend([0; 4]);

        let object = Object::read(&body).unwrap();
        assert_eq!(object.otype, 99);
        let properties: Vec<_> = object.properties().collect();
        assert_eq!(properties.len(), 2);
        assert_eq!(properties[0].key, 50);
        assert_eq!(
            number(types, properties[0].type_, properties[0].body),
            Some(120.0)
        );
        assert_eq!(
            number(types, properties[1].type_, properties[1].body),
            Some(1_000.0)
        );

        assert_eq!(number(types, 1, &(-3i32).to_ne_bytes()), Some(-3.0));
        assert_eq!(number(types, 4, &2.5f64.to_ne_bytes()), Some(2.5));
        assert_eq!(number(types, 4, &f64::NAN.to_ne_bytes()), None);
        assert_eq!(number(types, 3, &f32::INFINITY.to_ne_bytes()), None);
        assert_eq!(number(types, 2, &[0; 4]), None, "too short for a long");
        assert_eq!(number(types, 9, &[0; 8]), None, "not a number type");
        assert!(Object::read(&[0; 7]).is_none());
    }
}
