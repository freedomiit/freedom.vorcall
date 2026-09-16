namespace Vorcall.Server.Data;

// A sticker in the server-wide library. Like a sound the row is written before the bytes land,
// because the row id is what names the file.
public class Sticker
{
    public long Id { get; set; }

    public string Name { get; set; } = string.Empty;

    public long? UploaderId { get; set; }

    public string ContentType { get; set; } = string.Empty;

    public long Size { get; set; }

    // False until the whole body has streamed in: a sticker is referenced by definition — it is the
    // library — so an incomplete row would otherwise sit in everyone's picker and 404 on send.
    public bool Complete { get; set; }

    public DateTime CreatedAt { get; set; }
}
