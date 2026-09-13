namespace Vorcall.Server.Data;

// A file the sender's own client serves on demand. Unlike an Attachment the server keeps only
// this record and proxies the bytes as they flow, never storing them, so the file is readable
// only while OwnerId is online.
public class StreamedFile
{
    public long Id { get; set; }

    // The channel the file was offered in; a SendMessage may only link it to a message of that
    // channel.
    public long ChannelId { get; set; }

    // The client that holds the bytes; null once the account is gone, which leaves the record
    // permanently unreadable.
    public long? OwnerId { get; set; }

    public long? MessageId { get; set; }

    public string FileName { get; set; } = string.Empty;

    public string ContentType { get; set; } = string.Empty;

    public long Size { get; set; }

    public DateTime CreatedAt { get; set; }
}
