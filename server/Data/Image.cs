namespace Vorcall.Server.Data;

// Stored as a smallint; the purpose names of the upload endpoint's query string map to these in
// order (avatar, banner, server_icon, role_icon).
public enum ImagePurpose
{
    Avatar = 1,
    Banner = 2,
    ServerIcon = 3,
    RoleIcon = 4,
}

// An uploaded avatar, banner, server icon or role icon. Like an attachment the row is written
// before the bytes land; an image nothing references is swept with its file.
public class Image
{
    public long Id { get; set; }

    public ImagePurpose Purpose { get; set; }

    public long? UploaderId { get; set; }

    public string ContentType { get; set; } = string.Empty;

    public long Size { get; set; }

    public DateTime CreatedAt { get; set; }
}
