#![cfg(target_os = "linux")]

use gtk::glib::{ToVariant, Variant};

#[test]
fn variant_string_iterator_is_sound_in_optimized_builds() {
    let variant = Variant::array_from_iter::<String>([
        "solo".to_string().to_variant(),
        "memory".to_string().to_variant(),
        "tray".to_string().to_variant(),
    ]);

    let mut values = variant
        .array_iter_str()
        .expect("string array should expose VariantStrIter");
    assert_eq!(values.next(), Some("solo"));
    assert_eq!(values.next_back(), Some("tray"));
    assert_eq!(values.next(), Some("memory"));
    assert_eq!(values.next(), None);
}
