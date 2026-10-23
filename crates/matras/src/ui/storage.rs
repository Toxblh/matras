//! Storage preferences for the bounded HTTP/media cache: one group of the
//! preferences dialog.

use adw::prelude::*;

use crate::resource_cache::ResourceCache;

const GIB: u64 = 1024 * 1024 * 1024;

pub fn group(cache: ResourceCache) -> adw::PreferencesGroup {
    let limit = adw::SpinRow::with_range(0.0, 50.0, 0.25);
    limit.set_title("Media cache limit");
    limit.set_subtitle("Avatars, previews and opened attachments; 0 disables caching");
    limit.set_digits(2);
    limit.set_value(crate::background::cache_limit_bytes() as f64 / GIB as f64);
    limit.connect_value_notify({
        let cache = cache.clone();
        move |row| {
            let bytes = (row.value() * GIB as f64).round() as u64;
            crate::background::set_cache_limit_bytes(bytes);
            cache.set_limit(bytes);
        }
    });

    let usage = adw::ActionRow::builder()
        .title("Currently used")
        .subtitle("Calculating…")
        .build();
    let clear = gtk::Button::builder()
        .label("Clear")
        .valign(gtk::Align::Center)
        .build();
    clear.add_css_class("destructive-action");
    usage.add_suffix(&clear);

    crate::runtime::spawn(
        {
            let cache = cache.clone();
            async move { cache.size().await }
        },
        {
            let usage = usage.clone();
            move |bytes| usage.set_subtitle(&human_bytes(bytes))
        },
    );
    clear.connect_clicked({
        let cache = cache.clone();
        let usage = usage.clone();
        move |button| {
            button.set_sensitive(false);
            let button = button.clone();
            let cache = cache.clone();
            let usage = usage.clone();
            crate::runtime::spawn(async move { cache.clear().await }, move |()| {
                usage.set_subtitle("0 B");
                button.set_sensitive(true);
            });
        }
    });

    let group = adw::PreferencesGroup::builder()
        .title("Storage")
        .description("The limit is shared by all accounts; cached files expire automatically.")
        .build();
    group.add(&limit);
    group.add(&usage);
    group
}

fn human_bytes(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MiB", bytes as f64 / (1024 * 1024) as f64)
    } else if bytes >= 1024 {
        format!("{:.1} KiB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::human_bytes;

    #[test]
    fn formats_usage_for_people() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1536), "1.5 KiB");
        assert_eq!(human_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }
}
