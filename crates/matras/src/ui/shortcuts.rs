/// The inbox shortcut must leave GTK's inspector accelerator available.
pub(super) const INBOX_ACCELERATOR: &[&str] = &["<Control><Shift>m"];

#[cfg(test)]
mod tests {
    use super::INBOX_ACCELERATOR;

    #[test]
    fn inbox_shortcut_does_not_steal_the_gtk_inspector() {
        assert_ne!(INBOX_ACCELERATOR, &["<Control><Shift>i"]);
    }
}
