//! Output transforms, with `wl_output.transform` semantics.
//!
//! A compositor renders a rotated or flipped output into a buffer in the
//! panel's native orientation and applies the transform at presentation, so a
//! capture of that buffer arrives untransformed, and the physical (X-screen)
//! rectangle of the output is the *transformed* size. [`Transform`] maps a
//! point or rectangle in the buffer to where it lands on screen.
//!
//! The value is read the way `wl_surface.set_buffer_transform` defines it,
//! which is how compositors use `wl_output.geometry` too (it is the transform a
//! fullscreen client sets to be scanned out directly): a counter-clockwise
//! rotation, after a flip for the flipped variants, that the compositor has
//! applied to the *content* to produce the buffer. Showing the buffer means
//! undoing that, so [`point`](Transform::point) rotates the buffer *clockwise*
//! by the named angle. `ext_image_copy_capture_frame_v1.transform` ("the
//! transform that the compositor has applied to the buffer contents") is the
//! same value again. Checked against sway, where an output configured with
//! `transform 90` reports 270 on both, and this mapping puts its bar at the
//! bottom.

/// A Wayland transform value; see the module docs for which way it goes.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Transform {
    #[default]
    Normal,
    Rotate90,
    Rotate180,
    Rotate270,
    Flipped,
    Flipped90,
    Flipped180,
    Flipped270,
}

impl Transform {
    /// Whether the transform swaps the axes (a 90 or 270 degree rotation).
    pub fn swaps_axes(self) -> bool {
        matches!(
            self,
            Self::Rotate90 | Self::Rotate270 | Self::Flipped90 | Self::Flipped270
        )
    }

    /// The transformed size of a `w`x`h` buffer.
    pub fn size(self, w: u32, h: u32) -> (u32, u32) {
        if self.swaps_axes() { (h, w) } else { (w, h) }
    }

    /// Maps a rectangle in an untransformed `w`x`h` buffer to the transformed
    /// space (a 90 turns the buffer clockwise: its top-left corner goes to the
    /// top-right).
    pub fn rect(
        self,
        (x, y, rw, rh): (i32, i32, i32, i32),
        w: i32,
        h: i32,
    ) -> (i32, i32, i32, i32) {
        let (dw, dh) = if self.swaps_axes() {
            (rh, rw)
        } else {
            (rw, rh)
        };
        let (dx, dy) = match self {
            Self::Normal => (x, y),
            Self::Rotate90 => (h - y - rh, x),
            Self::Rotate180 => (w - x - rw, h - y - rh),
            Self::Rotate270 => (y, w - x - rw),
            Self::Flipped => (w - x - rw, y),
            Self::Flipped90 => (y, x),
            Self::Flipped180 => (x, h - y - rh),
            Self::Flipped270 => (h - y - rh, w - x - rw),
        };
        (dx, dy, dw, dh)
    }

    /// Maps a pixel in an untransformed `w`x`h` buffer to the transformed space.
    pub fn point(self, x: i32, y: i32, w: i32, h: i32) -> (i32, i32) {
        let (dx, dy, _, _) = self.rect((x, y, 1, 1), w, h);
        (dx, dy)
    }
}

#[cfg(test)]
mod tests {
    use super::Transform;

    // a 4x2 buffer: the corner pixels, and where each transform puts them
    const W: i32 = 4;
    const H: i32 = 2;
    const TL: (i32, i32) = (0, 0);
    const TR: (i32, i32) = (3, 0);
    const BL: (i32, i32) = (0, 1);
    const BR: (i32, i32) = (3, 1);

    fn corners(t: Transform) -> [(i32, i32); 4] {
        [TL, TR, BL, BR].map(|(x, y)| t.point(x, y, W, H))
    }

    #[test]
    fn size_swaps_for_quarter_turns() {
        assert_eq!(Transform::Normal.size(4, 2), (4, 2));
        assert_eq!(Transform::Rotate90.size(4, 2), (2, 4));
        assert_eq!(Transform::Rotate180.size(4, 2), (4, 2));
        assert_eq!(Transform::Flipped270.size(4, 2), (2, 4));
    }

    #[test]
    fn rotate90_turns_the_buffer_clockwise_on_screen() {
        // the content was turned 90 counter-clockwise into the buffer, so the
        // buffer turns clockwise to show it: top-left ends up top-right
        assert_eq!(
            corners(Transform::Rotate90),
            [(1, 0), (1, 3), (0, 0), (0, 3)]
        );
    }

    #[test]
    fn rotate270_turns_the_buffer_counter_clockwise_on_screen() {
        assert_eq!(
            corners(Transform::Rotate270),
            [(0, 3), (0, 0), (1, 3), (1, 0)]
        );
    }

    #[test]
    fn rotate180_mirrors_both_axes() {
        assert_eq!(
            corners(Transform::Rotate180),
            [(3, 1), (0, 1), (3, 0), (0, 0)]
        );
    }

    #[test]
    fn flipped_mirrors_horizontally() {
        assert_eq!(
            corners(Transform::Flipped),
            [(3, 0), (0, 0), (3, 1), (0, 1)]
        );
        assert_eq!(
            corners(Transform::Flipped180),
            [(0, 1), (3, 1), (0, 0), (3, 0)]
        );
    }

    #[test]
    fn flipped_quarter_turns_transpose() {
        assert_eq!(
            corners(Transform::Flipped90),
            [(0, 0), (0, 3), (1, 0), (1, 3)]
        );
        assert_eq!(
            corners(Transform::Flipped270),
            [(1, 3), (1, 0), (0, 3), (0, 0)]
        );
    }

    #[test]
    fn every_transform_is_a_bijection_onto_the_transformed_size() {
        for t in [
            Transform::Normal,
            Transform::Rotate90,
            Transform::Rotate180,
            Transform::Rotate270,
            Transform::Flipped,
            Transform::Flipped90,
            Transform::Flipped180,
            Transform::Flipped270,
        ] {
            let (tw, th) = t.size(W as u32, H as u32);
            let mut seen = vec![false; (tw * th) as usize];
            for y in 0..H {
                for x in 0..W {
                    let (dx, dy) = t.point(x, y, W, H);
                    assert!(
                        dx >= 0 && dx < tw as i32 && dy >= 0 && dy < th as i32,
                        "{t:?}"
                    );
                    let i = (dy * tw as i32 + dx) as usize;
                    assert!(!seen[i], "{t:?} maps two pixels to {dx},{dy}");
                    seen[i] = true;
                }
            }
        }
    }

    #[test]
    fn rect_covers_its_points() {
        let t = Transform::Rotate90;
        let (x, y, w, h) = t.rect((1, 0, 2, 1), W, H);
        // the two pixels (1,0) and (2,0) land at (1,1) and (1,2)
        assert_eq!(t.point(1, 0, W, H), (1, 1));
        assert_eq!(t.point(2, 0, W, H), (1, 2));
        assert_eq!((x, y, w, h), (1, 1, 1, 2));
    }
}
