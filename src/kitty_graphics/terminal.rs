use super::{
    encode_delete_image, encode_graphics_output, image_signature_from_descriptor,
    image_signature_from_placement, image_transfer_estimated_size, kitty_format_code,
    terminal_pixels, HostCellSize, HostGraphicsCache, HostPlacement, HostSourceKey,
    HEADLESS_GRAPHICS_TRANSACTION_BUDGET,
};
use crate::layout::PaneId;
use ratatui::layout::Rect;
use std::collections::HashSet;
use std::sync::Arc;

/// One terminal's visible scene, emitted through the existing terminal-ANSI frame codec.
/// Cache changes are committed by the caller only after that frame enters the writer queue.
pub(crate) fn output(
    runtime: &crate::terminal::TerminalRuntime,
    terminal_id: &str,
    area: Rect,
    cell_size: HostCellSize,
    cache: &mut HostGraphicsCache,
    pixels: &terminal_pixels::TerminalPixels,
    notify: impl FnOnce() + Clone + Send + 'static,
) -> Vec<u8> {
    let source = |image_id| HostSourceKey::AttachedTerminal {
        terminal_id: terminal_id.to_owned(),
        image_id,
    };
    let mut accepted = HashSet::new();
    let mut remaining = HEADLESS_GRAPHICS_TRANSACTION_BUDGET;
    let placements = runtime.kitty_image_placements_with_data_filter(|descriptor| {
        if accepted.contains(&descriptor.image_id) || accepted.len() >= 4096 {
            return false;
        }
        let signature =
            image_signature_from_descriptor(descriptor, kitty_format_code(descriptor.format));
        if cache
            .sources
            .get(&source(descriptor.image_id))
            .and_then(|id| cache.images.get(id))
            == Some(&signature)
        {
            accepted.insert(descriptor.image_id);
            return false;
        }
        let size = image_transfer_estimated_size(descriptor.data_len);
        if size > remaining {
            return false;
        }
        remaining -= size;
        accepted.insert(descriptor.image_id);
        !descriptor.source_file
    });
    if placements.is_empty() {
        return cache.clear_bytes();
    }
    let scrollback_offset = runtime
        .scroll_metrics()
        .map(|metrics| metrics.offset_from_bottom as u32)
        .unwrap_or(0);
    let cell_size = if cell_size.is_known() {
        cell_size
    } else {
        HostCellSize {
            width_px: 8,
            height_px: 16,
        }
    };
    let placements = placements
        .into_iter()
        .filter(|placement| accepted.contains(&placement.image_id))
        .take(4096)
        .filter_map(|placement| {
            let signature =
                image_signature_from_placement(&placement, kitty_format_code(placement.format));
            let raw_data = if cache.images.get(&placement.image_id) != Some(&signature) {
                if let Some(source) = &placement.source_file {
                    match pixels.request(
                        terminal_id,
                        placement.image_id,
                        signature,
                        Arc::clone(source),
                        notify.clone(),
                    ) {
                        terminal_pixels::Pixels::Ready(data) => Some(data),
                        terminal_pixels::Pixels::Pending => None,
                        terminal_pixels::Pixels::Unavailable => return None,
                    }
                } else {
                    None
                }
            } else {
                None
            };
            Some(HostPlacement {
                raw_data,
                // Direct terminal streams have their own image namespace; the legacy pane-based
                // ID allocator is bypassed by the explicit host_image_id below.
                pane_id: PaneId::from_raw(0),
                host_image_id: Some(placement.image_id),
                area,
                cell_size,
                source_key: source(placement.image_id),
                placement,
                scrollback_offset,
            })
        })
        .collect::<Vec<_>>();
    let mut bytes = encode_graphics_output(cache, &placements).into_inline_bytes();
    // Unlike the shell's bounded off-screen cache, attach only retains visible images.
    let retained = cache.sources.values().copied().collect::<HashSet<_>>();
    cache.images.retain(|id, _| {
        if retained.contains(id) {
            true
        } else {
            encode_delete_image(&mut bytes, *id);
            false
        }
    });
    bytes
}
