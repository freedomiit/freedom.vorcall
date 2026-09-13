namespace Vorcall.Server.Data;

// A clip in the server-wide soundpad library. Like an image the row is written before the bytes
// land, because the row id is what names the file; duration comes off the container's frame count
// at upload time, so nothing ever has to decode a sample to know how long a clip runs.
public class Sound
{
    public long Id { get; set; }

    public string Name { get; set; } = string.Empty;

    public long? UploaderId { get; set; }

    public string ContentType { get; set; } = string.Empty;

    public long Size { get; set; }

    public int DurationMs { get; set; }

    // False until the whole body has streamed in. The row exists before its bytes do, and unlike
    // an image a clip is referenced by definition — it is the library — so an incomplete row would
    // otherwise sit in everyone's soundpad forever and 404 on play.
    public bool Complete { get; set; }

    public DateTime CreatedAt { get; set; }
}
