pub(crate) const VIDEO_EXTENSIONS: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "webm", "flv", "wmv", "m4v", "mpg", "mpeg", "ts", "mts", "vob",
    "ogv", "3gp",
];

pub(crate) const IMAGE_EXTENSIONS: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "ico", "tiff", "webp", "avif", "pnm", "tga",
];

pub(crate) fn is_video(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    VIDEO_EXTENSIONS.contains(&ext.as_str())
}

pub(crate) fn is_image(name: &str) -> bool {
    let ext = name.rsplit('.').next().unwrap_or("").to_lowercase();
    IMAGE_EXTENSIONS.contains(&ext.as_str())
}
