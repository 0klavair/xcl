use super::Selector;

#[test]
fn selector_parsing() {
    assert_eq!(Selector::parse(""), Selector::Auto);
    assert_eq!(Selector::parse(" Auto "), Selector::Auto);
    assert_eq!(Selector::parse("WARP"), Selector::Warp);
    assert_eq!(Selector::parse("1"), Selector::Index(1));
    assert_eq!(Selector::parse("Radeon"), Selector::Name("radeon".into()));
    assert_eq!(Selector::parse(" RTX 3080 "), Selector::Name("rtx 3080".into()));
}

#[test]
fn selector_matching() {
    let nv = "NVIDIA GeForce RTX 3080";
    let amd = "AMD Radeon(TM) Graphics";
    assert!(Selector::Auto.allows(0, nv) && Selector::Auto.allows(1, amd));
    assert!(!Selector::Warp.allows(0, nv));
    assert!(Selector::Index(1).allows(1, amd) && !Selector::Index(1).allows(0, nv));
    let radeon = Selector::parse("radeon");
    assert!(radeon.allows(1, amd) && !radeon.allows(0, nv));
    assert!(Selector::parse("geforce rtx").allows(0, nv));
}
