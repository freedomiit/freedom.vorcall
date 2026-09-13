namespace Vorcall.Server.Data;

// An uploaded file. The row is written before the bytes land, and stays unlinked (MessageId
// null) until a SendMessage names its id; unlinked rows are swept with their files.
public class Attachment
{
    public long Id { get; set; }

    // The channel the upload was made for; a SendMessage may only link it to a message of that
    // channel.
    public long ChannelId { get; set; }

    public long? UploaderId { get; set; }

    public long? MessageId { get; set; }

    // Metadata only, echoed back to clients: the file on disk is always <id>.<ext>.
    public string FileName { get; set; } = string.Empty;

    public string ContentType { get; set; } = string.Empty;

    public long Size { get; set; }

    // False until the whole body has streamed in. The row exists before its bytes do, so an
    // incomplete row is an upload still in flight — or one that died mid-flight — and the short
    // unlinked-upload TTL must not sweep it.
    public bool Complete { get; set; }

    public DateTime CreatedAt { get; set; }
}
