#[macro_use]
extern crate cpython;
#[macro_use]
extern crate lazy_static;
extern crate bytes;
extern crate packet_sniffer;
extern crate protocol16;

use std::fs::File;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Mutex;
use std::thread;

use cpython::{PyDict, PyList, PyObject, PyResult, Python, PythonObject, ToPyObject};
use log::*;
use simplelog::*;

use packet_sniffer::UdpPacket;

mod game_protocol;
mod meter;

use meter::ZoneStats;
use meter::PlayerEvents;

lazy_static! {
    static ref METER: Mutex<meter::Meter> = Mutex::new(meter::Meter::new());
}

impl ToPyObject for meter::PlayerStatistics {
    type ObjectType = PyObject;
    fn to_py_object(&self, py: Python) -> Self::ObjectType {
        let dict = PyDict::new(py);

        dict.set_item(py, "player", self.player.to_py_object(py))
            .unwrap();
        dict.set_item(py, "damage", self.damage.to_py_object(py))
            .unwrap();
        dict.set_item(py, "time_in_combat", self.time_in_combat.to_py_object(py))
            .unwrap();
        dict.set_item(py, "dps", self.dps.to_py_object(py)).unwrap();

        dict.into_object()
    }
}

fn get_zone_session(py: Python) -> PyResult<PyList> {
    let meter = &mut METER.lock().unwrap();
    meter.get_zone_session().map_or_else(
        || Ok(PyList::new(py, Vec::<PyObject>::new().as_slice())),
        |v| {
            Ok(PyList::new(
                py,
                v.iter()
                    .map(|s| s.into_py_object(py))
                    .collect::<Vec<PyObject>>()
                    .as_slice(),
            ))
        },
    )
}

fn new_zone_session(_py: Python) -> PyResult<u32> {
    let meter = &mut METER.lock().unwrap();

    meter.new_zone_session();

    Ok(0)
}

fn initialize(_py: Python) -> PyResult<u32> {
    CombinedLogger::init(vec![WriteLogger::new(
        LevelFilter::Trace,
        Config::default(),
        File::create("damage-meter.log").unwrap(),
    )])
    .unwrap();

    thread::spawn(move || {
        let (tx, rx): (Sender<UdpPacket>, Receiver<UdpPacket>) = channel();

        packet_sniffer::receive(tx);
        info!("Listening to network packets...");
        
        loop {
            if let Ok(packet) = rx.recv() {
                if packet.destination_port != 5056 && packet.source_port != 5056 {
                    continue;
                }
                let meter = &mut METER.lock().unwrap();
                register_messages(meter, &game_protocol::decode(&packet.payload));
            }
        }
    });

    Ok(0)
}

fn register_messages(meter: &mut meter::Meter, messages: &Vec<game_protocol::Message>) {
    messages
        .iter()
        .for_each(|message| register_message(meter, &message));
}

fn register_message(events: &mut PlayerEvents, message: &game_protocol::Message) {
    debug!("Found message {:?}", message);
    match message {
        game_protocol::Message::Leave(msg) => events.register_leave(msg.source).unwrap_or(()),
        game_protocol::Message::NewCharacter(msg) => {
            events.register_player(&msg.character_name, msg.source)
        }
        game_protocol::Message::CharacterStats(msg) => {
            events.register_main_player(&msg.character_name, msg.source)
        }
        game_protocol::Message::HealthUpdate(msg) => events
            .register_damage_dealt(msg.target, msg.value)
            .unwrap_or(()),
        game_protocol::Message::RegenerationHealthChanged(msg) => {
            match msg.regeneration_rate {
                Some(_) => events.register_combat_leave(msg.source).unwrap_or(()),
                None => events.register_combat_enter(msg.source).unwrap_or(()),
            }
        },
        game_protocol::Message::Died(msg) => {
            events.register_combat_leave(msg.source).unwrap_or(())
        }
        _ => {}
    }
}

py_module_initializer!(libmeter, initliblibmeter, PyInit_libmeter, |py, m| {
    m.add(py, "__doc__", "This module is implemented in Rust")?;
    m.add(py, "initialize", py_fn!(py, initialize()))?;
    m.add(py, "new_zone_session", py_fn!(py, new_zone_session()))?;
    m.add(
        py,
        "get_zone_session",
        py_fn!(py, get_zone_session()),
    )?;
    Ok(())
});

#[cfg(test)]
mod tests {
    use super::*;
    use cpython::{PyFloat, PyUnicode};

    use game_protocol::message;
    use game_protocol::Message;

    mod helpers {
        use super::*;

        pub fn register(message: Message) {
            let meter = &mut METER.lock().unwrap();
            r(meter, &message);
        }

        fn r(meter: &mut meter::Meter, message: &game_protocol::Message) {
            register_message(meter, &message);
        }
    }

    trait Testing {
        fn new() -> Self;
    }

    impl Testing for message::NewCharacter {
        fn new() -> Self {
            Self {
                source: 1,
                character_name: String::from("CH1"),
                health: 10.0,
                max_health: 10.0,
                energy: 1.0,
                max_energy: 1.0,
            }
        }
    }

    impl Testing for message::HealthUpdate {
        fn new() -> Self {
            Self {
                source: 2,
                target: 1,
                value: -10.0,
            }
        }
    }

    impl Testing for message::RegenerationHealthChanged {
        fn new() -> Self {
            Self {
                source: 1,
                health: 10.0,
                max_health: 10.0,
                regeneration_rate: Some(1.0),
            }
        }
    }

    #[test]
    fn test_empty_session() {
        let guard = Python::acquire_gil();
        let py = guard.python();
        assert_eq!(get_zone_session(py).unwrap().len(py), 0);
    }

    #[test]
    fn test_new_player_appears() {
        let guard = Python::acquire_gil();
        let py = guard.python();

        helpers::register(Message::NewCharacter(message::NewCharacter::new()));

        let zone_session = get_zone_session(py).unwrap();
        assert_eq!(zone_session.len(py), 1);
    }

    #[test]
    fn test_new_player_stats() {
        let guard = Python::acquire_gil();
        let py = guard.python();

        helpers::register(Message::NewCharacter(message::NewCharacter::new()));

        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(dict.len(py), 4);
        assert_eq!(
            dict.get_item(py, &"player")
                .unwrap()
                .cast_as::<PyUnicode>(py)
                .unwrap()
                .to_string_lossy(py),
            "CH1"
        );
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );
        assert_eq!(
            dict.get_item(py, &"time_in_combat")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );
        assert_eq!(
            dict.get_item(py, &"dps")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );
    }

    #[test]
    fn test_new_player_damage() {
        let guard = Python::acquire_gil();
        let py = guard.python();

        helpers::register(Message::NewCharacter(message::NewCharacter::new()));

        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(
            dict.get_item(py, &"player")
                .unwrap()
                .cast_as::<PyUnicode>(py)
                .unwrap()
                .to_string_lossy(py),
            "CH1"
        );
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );

        helpers::register(Message::HealthUpdate(message::HealthUpdate::new()));

        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            10.0
        );
    }

    #[test]
    fn test_new_player_damage_reset() {
        let guard = Python::acquire_gil();
        let py = guard.python();

        helpers::register(Message::NewCharacter(message::NewCharacter::new()));

        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(
            dict.get_item(py, &"player")
                .unwrap()
                .cast_as::<PyUnicode>(py)
                .unwrap()
                .to_string_lossy(py),
            "CH1"
        );
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );

        helpers::register(Message::HealthUpdate(message::HealthUpdate::new()));

        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            10.0
        );


        new_zone_session(py).unwrap();
        let zone_session = get_zone_session(py).unwrap();
        let stat = zone_session.get_item(py, 0);
        let dict = stat.cast_as::<PyDict>(py).unwrap();
        assert_eq!(
            dict.get_item(py, &"damage")
                .unwrap()
                .cast_as::<PyFloat>(py)
                .unwrap()
                .value(py),
            0.0
        );
    }

}
