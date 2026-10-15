//! The testing helpers that look at any window, whoever made it: the
//! children of a parent, their classes, sizes and places. A host-like test
//! with plugin binaries of its own uses them on windows another binary
//! made, where the helpers for this crate's own windows refuse.

#![cfg(windows)]

use manguebyte_window::win32::testing::{self, TopLevel, WindowClass};
use manguebyte_window::{Handled, PhysicalSize, Window, WindowCx, WindowOptions};

#[test]
fn a_child_window_is_found_with_its_class_size_and_place() {
    let parent = TopLevel::new(120, 90).expect("a desktop");
    let top = parent.handle().expect("open");
    assert_eq!(testing::children_of(top), Vec::<usize>::new());
    assert_eq!(testing::parent_of_any(top), None);
    assert_eq!(testing::client_size_of(top), Some((120, 90)));
    assert_eq!(testing::position_in_parent(top), None, "a top-level window");
    // The test parent's own class, registered by this executable.
    let top_class = testing::class_of(top).expect("a window");
    assert!(testing::class_exists(&top_class));
    assert_ne!(top_class.name, testing::window_class_name());

    let window = Window::open(
        parent.parent(),
        WindowOptions::new(PhysicalSize::new(64, 48)),
        |_: &WindowCx<'_>, _| Handled::Yes,
    )
    .expect("a child window");
    let child = window.raw_handle().expect("open");
    assert_eq!(testing::children_of(top), [child]);
    assert_eq!(testing::children_of(child), Vec::<usize>::new());
    assert_eq!(testing::parent_of_any(child), Some(top));
    assert_eq!(testing::parent_of_any(child), testing::parent_of(child));
    assert_eq!(testing::client_size_of(child), Some((64, 48)));
    assert_eq!(testing::position_in_parent(child), Some((0, 0)));
    let class = testing::class_of(child).expect("a window");
    assert_eq!(class.name, testing::window_class_name());
    assert_ne!(class.module, 0);
    assert_eq!(class.module, top_class.module, "both from this executable");
    assert!(testing::class_exists(&class));

    // The same name under another module is another class.
    let elsewhere = WindowClass {
        name: class.name.clone(),
        module: class.module + 0x1_0000,
    };
    assert!(!testing::class_exists(&elsewhere));

    drop(window);
    assert!(!testing::is_window(child));
    assert_eq!(testing::children_of(top), Vec::<usize>::new());
    assert_eq!(testing::class_of(child), None);
    assert_eq!(testing::client_size_of(child), None);
    assert_eq!(testing::position_in_parent(child), None);
    assert_eq!(testing::parent_of_any(child), None);
    // The last window of the class is gone, so is the class.
    assert!(!testing::class_exists(&class));
    assert!(!testing::class_registered());
}

#[test]
fn values_that_are_no_window_give_nothing() {
    for handle in [0, 1, usize::MAX, 0xdead_0000] {
        assert_eq!(testing::children_of(handle), Vec::<usize>::new());
        assert_eq!(testing::class_of(handle), None);
        assert_eq!(testing::parent_of_any(handle), None);
        assert_eq!(testing::client_size_of(handle), None);
        assert_eq!(testing::position_in_parent(handle), None);
    }
    assert!(!testing::class_exists(&WindowClass {
        name: String::new(),
        module: 0,
    }));
    assert!(!testing::class_exists(&WindowClass {
        name: "No such class \u{1F986}".to_owned(),
        module: 0x1000,
    }));
}
