//! A picture: an image fitted to the widget's rectangle.

use manguebyte_gfx::{Image, Rect, Sampling, Scene, Size};

use crate::access::{AccessNode, Role};
use crate::widget::{AccessCx, MeasureCx, PaintCx, Widget};

/// How an [`ImageView`] fits its image to its rectangle. Whatever reaches
/// past the rectangle is cut off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Fit {
    /// As large as fits whole, keeping its shape, centred: room is left
    /// on two sides if the shapes differ.
    #[default]
    Contain,
    /// As small as covers the rectangle, keeping its shape, centred: two
    /// sides are cut if the shapes differ.
    Cover,
    /// Exactly the rectangle, out of shape if the shapes differ.
    Stretch,
    /// At its own size, centred, cut where the rectangle is smaller.
    None,
}

/// A picture with a name for assistive technology: one image, or the same
/// picture at several densities, of which the one that suits the scale it
/// is drawn at is used.
///
/// An image's density is how many of its pixels make one logical pixel: 1
/// for an image drawn at its own size on a screen of scale 1, 2 for one
/// made for twice that, as a "@2x" image is. The natural size of the
/// widget is the first image's pixels over its density. Drawn at scale 2,
/// a density 2 image at its own size ([`Fit::None`], or any fit that comes
/// to the same size) puts each of its pixels on one device pixel, with
/// edges on whole device pixels, so it is as sharp as the image.
pub struct ImageView {
    name: String,
    /// The pictures and their densities, the first giving the natural
    /// size.
    images: Vec<(Image, f32)>,
    fit: Fit,
    sampling: Sampling,
}

/// A picture named `name` showing `image`, of density 1, fitted by
/// [`Fit::Contain`].
///
/// ```
/// use manguebyte_gfx::{Color, Image, Pixmap, Size};
/// use manguebyte_ui::testing::Harness;
/// use manguebyte_ui::{Fit, Role, image};
///
/// // A 4 by 4 image made for scale 2: 2 by 2 logical pixels.
/// let mut pixels = Pixmap::new(4, 4).expect("small");
/// pixels.fill(Color::hex(0x3D8BFF));
/// let logo = image("Logo", Image::new(pixels)).density(2.0).fit(Fit::None);
/// let mut ui = Harness::new(logo, Size::new(2.0, 2.0), 2.0);
/// ui.frame();
/// let found = ui.get(Role::Image, "Logo");
/// assert_eq!(found.bounds.width, 2.0);
/// assert_eq!(ui.pixels().pixel(3, 3).expect("inside").to_array(), [0x3D, 0x8B, 0xFF, 0xFF]);
/// ```
pub fn image(name: impl Into<String>, image: Image) -> ImageView {
    ImageView {
        name: name.into(),
        images: vec![(image, 1.0)],
        fit: Fit::default(),
        sampling: Sampling::Bilinear,
    }
}

impl ImageView {
    /// The same picture, its first image of `density` pixels per logical
    /// pixel. A density that is not a positive finite number counts as 1.
    #[must_use]
    pub fn density(mut self, density: f32) -> ImageView {
        if let Some(first) = self.images.first_mut() {
            first.1 = clean_density(density);
        }
        self
    }

    /// The same picture with `image`, of `density`, as another version of
    /// it, used when the interface is drawn at a scale that suits it
    /// better.
    #[must_use]
    pub fn with_density(mut self, image: Image, density: f32) -> ImageView {
        self.images.push((image, clean_density(density)));
        self
    }

    /// The same picture fitted by `fit`.
    #[must_use]
    pub fn fit(mut self, fit: Fit) -> ImageView {
        self.fit = fit;
        self
    }

    /// The same picture, sampled by `sampling` when it is drawn at another
    /// size than its own pixels (bilinear unless set). Drawn at exactly its
    /// own pixels it is copied as it is, whatever this says.
    #[must_use]
    pub fn sampling(mut self, sampling: Sampling) -> ImageView {
        self.sampling = sampling;
        self
    }

    /// The image that suits `scale`: the least dense that has at least as
    /// many pixels as the screen, or else the densest.
    fn pick(&self, scale: f32) -> Option<&(Image, f32)> {
        self.images
            .iter()
            .filter(|(_, density)| *density >= scale)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .or_else(|| self.images.iter().max_by(|a, b| a.1.total_cmp(&b.1)))
    }

    /// The size the picture has at its own pixels, in logical pixels.
    fn natural(&self) -> Size {
        self.images
            .first()
            .map_or(Size::default(), |(image, density)| {
                Size::new(
                    image.width() as f32 / density,
                    image.height() as f32 / density,
                )
            })
    }
}

fn clean_density(density: f32) -> f32 {
    if density.is_finite() && density > 0.0 {
        density
    } else {
        1.0
    }
}

/// `value` on the nearest device pixel at `scale`.
fn on_pixel(value: f32, scale: f32) -> f32 {
    (f64::from(value) * f64::from(scale)).round() as f32 / scale
}

/// Where a picture of natural size `natural` goes in `rect`, fitted by
/// `fit`, its corner on a device pixel at `scale`.
fn place(natural: Size, rect: Rect, fit: Fit, scale: f32) -> Option<Rect> {
    if !(natural.width > 0.0 && natural.height > 0.0) || rect.is_empty() {
        return None;
    }
    let size = match fit {
        Fit::Stretch => return Some(rect),
        Fit::None => natural,
        Fit::Contain | Fit::Cover => {
            let across = rect.width / natural.width;
            let down = rect.height / natural.height;
            let factor = if fit == Fit::Contain {
                across.min(down)
            } else {
                across.max(down)
            };
            Size::new(natural.width * factor, natural.height * factor)
        }
    };
    let x = on_pixel(rect.x + (rect.width - size.width) * 0.5, scale);
    let y = on_pixel(rect.y + (rect.height - size.height) * 0.5, scale);
    Some(Rect::new(x, y, size.width, size.height))
}

impl Widget for ImageView {
    fn access(&self, _cx: &AccessCx<'_>) -> AccessNode {
        AccessNode::new(Role::Image, self.name.as_str())
    }

    fn measure(&mut self, _cx: &MeasureCx<'_>) -> Size {
        self.natural()
    }

    fn paint(&mut self, cx: &PaintCx<'_>, scene: &mut Scene) {
        let rect = cx.rect();
        let scale = cx.scale();
        let Some(to) = place(self.natural(), rect, self.fit, scale) else {
            return;
        };
        let Some((picture, _)) = self.pick(scale) else {
            return;
        };
        // At exactly its own pixels, on whole device pixels, a copy.
        let device = |v: f32| f64::from(v) * f64::from(scale);
        let exact = |length: f32, pixels: u32| (device(length) - f64::from(pixels)).abs() < 1e-3;
        let whole = |v: f32| (device(v) - device(v).round()).abs() < 1e-3;
        let sampling = if exact(to.width, picture.width())
            && exact(to.height, picture.height())
            && whole(to.x)
            && whole(to.y)
        {
            Sampling::Nearest
        } else {
            self.sampling
        };
        let inside = rect.intersect(&to) == Some(to);
        if inside {
            scene.image(picture, to, sampling);
        } else {
            scene.clipped(rect, |scene| scene.image(picture, to, sampling));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_fit_places_the_picture_as_it_says() {
        let rect = Rect::new(0.0, 0.0, 100.0, 50.0);
        let natural = Size::new(20.0, 20.0);
        assert_eq!(
            place(natural, rect, Fit::Contain, 1.0),
            Some(Rect::new(25.0, 0.0, 50.0, 50.0))
        );
        assert_eq!(
            place(natural, rect, Fit::Cover, 1.0),
            Some(Rect::new(0.0, -25.0, 100.0, 100.0))
        );
        assert_eq!(place(natural, rect, Fit::Stretch, 1.0), Some(rect));
        assert_eq!(
            place(natural, rect, Fit::None, 1.0),
            Some(Rect::new(40.0, 15.0, 20.0, 20.0))
        );
        // The corner lands on a device pixel.
        let odd = Rect::new(0.0, 0.0, 21.0, 21.0);
        assert_eq!(
            place(natural, odd, Fit::None, 2.0),
            Some(Rect::new(0.5, 0.5, 20.0, 20.0))
        );
        assert_eq!(place(Size::default(), rect, Fit::None, 1.0), None);
    }
}
