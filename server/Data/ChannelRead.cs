namespace Vorcall.Server.Data;

// One account's read cursor in one text channel: everything above it is unread. There is no
// membership row any more, so the cursor is the only per-account row a channel has, and it is
// created lazily — a missing row means the whole channel is unread.
public class ChannelRead
{
    public long ChannelId { get; set; }

    public long UserId { get; set; }

    public long LastReadMessageId { get; set; }
}
