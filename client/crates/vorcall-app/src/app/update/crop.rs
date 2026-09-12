//! The crop adjuster: the one dialog every picked picture goes through, whoever
//! picked it.
//!
//! Neither the decode nor the crop runs on the UI thread. Between the two the
//! dialog holds the file exactly as it was picked and draws it by scissoring the
//! preview, so a drag costs a redraw and nothing is encoded until the user
//! presses through.

use iced::{Point, Task};
use vorcall_core::connection::Blob;
use vorcall_core::images::ImagePurpose;

use crate::app::App;
use crate::app::message::{CropMsg, Message, ToastKind};
use crate::app::state::crop::{CropDrag, CropState, FRAME_WIDTH, ZOOM_MAX, ZOOM_MIN, region};
use crate::app::state::ui::Dialog;
use crate::app::update::{admin, settings};
use crate::workers::images;

pub fn update(app: &mut App, message: CropMsg) -> Task<Message> {
    match message {
        CropMsg::Open { purpose, bytes } => open(purpose, bytes),
        CropMsg::Ready {
            purpose,
            bytes,
            handle,
            source,
        } => {
            // A second pick replaces the dialog rather than stacking on it.
            app.ui.dialog = Some(Dialog::CropImage {
                purpose,
                bytes,
                handle,
                source,
                // The centred maximum: pressing straight through takes as much
                // of the picture as the shape allows.
                crop: CropState {
                    centre: (0.5, 0.5),
                    zoom: ZOOM_MIN,
                },
                drag: None,
            });
            Task::none()
        }
        CropMsg::Failed(error) => {
            app.toast(ToastKind::Error, error);
            Task::none()
        }
        CropMsg::PanStart => {
            if let Some(Dialog::CropImage { crop, drag, .. }) = app.dialog_mut() {
                // A press carries no position of its own; the first move fills
                // the anchor in.
                *drag = Some(CropDrag {
                    from: None,
                    centre: crop.centre,
                });
            }
            Task::none()
        }
        CropMsg::PanMove(at) => {
            if let Some(Dialog::CropImage {
                purpose,
                source,
                crop,
                drag,
                ..
            }) = app.dialog_mut()
            {
                pan(*purpose, *source, crop, drag, at);
            }
            Task::none()
        }
        CropMsg::PanEnd => {
            if let Some(Dialog::CropImage { drag, .. }) = app.dialog_mut() {
                *drag = None;
            }
            Task::none()
        }
        CropMsg::Zoom(zoom) => {
            if let Some(Dialog::CropImage { crop, .. }) = app.dialog_mut() {
                crop.zoom = zoom.clamp(ZOOM_MIN, ZOOM_MAX);
            }
            Task::none()
        }
        CropMsg::Apply => apply(app),
        CropMsg::Applied {
            purpose,
            content_type,
            bytes,
        } => match purpose {
            ImagePurpose::Avatar | ImagePurpose::Banner => {
                settings::upload_cropped(app, purpose, content_type, bytes)
            }
            ImagePurpose::ServerIcon | ImagePurpose::RoleIcon => {
                admin::upload_cropped(app, purpose, content_type, bytes)
            }
        },
    }
}

/// Decodes the picked file for the preview, in the source's own pixels: the
/// rectangle the dialog builds is in those, and so is the crop the upload cuts
/// out. A ceiling of zero is what [`images::decode`] takes for "no ceiling".
fn open(purpose: ImagePurpose, bytes: Blob) -> Task<Message> {
    Task::perform(
        tokio::task::spawn_blocking(move || {
            let decoded = images::decode(&bytes, 0);
            decoded.map(|(width, height, rgba)| (bytes, width, height, rgba))
        }),
        move |joined| match joined.unwrap_or_else(|e| Err(e.to_string())) {
            Ok((bytes, width, height, rgba)) => Message::Crop(CropMsg::Ready {
                purpose,
                bytes,
                handle: iced::widget::image::Handle::from_rgba(width, height, rgba),
                source: (width, height),
            }),
            Err(error) => Message::Crop(CropMsg::Failed(error)),
        },
    )
}

/// Moves the crop with the pointer: a drag to the right takes the picture right,
/// which is the window over it going left.
fn pan(
    purpose: ImagePurpose,
    source: (u32, u32),
    crop: &mut CropState,
    drag: &mut Option<CropDrag>,
    at: Point,
) {
    // A move with no press behind it is the pointer passing over the frame.
    let Some(anchor) = drag.as_mut() else {
        return;
    };
    let Some(from) = anchor.from else {
        anchor.from = Some(at);
        anchor.centre = crop.centre;
        return;
    };

    // The frame draws the whole region across its own width, and the two share
    // an aspect, so one scale answers for both directions.
    let rect = region(source, purpose.max_size(), *crop);
    let per_frame_pixel = rect.width as f32 / FRAME_WIDTH;
    let across = (at.x - from.x) * per_frame_pixel / source.0.max(1) as f32;
    let down = (at.y - from.y) * per_frame_pixel / source.1.max(1) as f32;

    crop.centre = (
        (anchor.centre.0 - across).clamp(0.0, 1.0),
        (anchor.centre.1 - down).clamp(0.0, 1.0),
    );
}

/// Cuts the crop out and scales it into the box its purpose allows, off the UI
/// thread. The dialog goes away with the press: the crop it held is in the
/// rectangle now, and nothing it kept survives.
fn apply(app: &mut App) -> Task<Message> {
    let Some(Dialog::CropImage {
        purpose,
        bytes,
        source,
        crop,
        ..
    }) = app.ui.dialog.clone()
    else {
        return Task::none();
    };
    app.ui.dialog = None;

    let rect = region(source, purpose.max_size(), crop);
    Task::perform(
        tokio::task::spawn_blocking(move || images::crop_for_upload(&bytes, purpose, rect)),
        move |joined| match joined.unwrap_or_else(|e| Err(e.to_string())) {
            Ok((content_type, bytes)) => Message::Crop(CropMsg::Applied {
                purpose,
                content_type,
                bytes: Blob::from(bytes),
            }),
            Err(error) => Message::Crop(CropMsg::Failed(error)),
        },
    )
}
