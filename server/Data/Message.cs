namespace Vorcall.Server.Data;

public class Message
{
    public long Id { get; set; }

    public string Author { get; set; } = string.Empty;

    public string Text { get; set; } = string.Empty;

    public DateTime SentAt { get; set; }

    public long ChannelId { get; set; }

    // Null for the messages that predate accounts; they keep Author as their only identity.
    public long? UserId { get; set; }

    public DateTime? EditedAt { get; set; }

    // A tombstone: the row keeps its id, author and channel so replies to it still resolve, but
    // its text, mentions, reactions and attachments are gone.
    public DateTime? DeletedAt { get; set; }

    // No foreign key: the target is only ever read back by id, and a reply must survive its
    // target being deleted (which tombstones the row rather than removing it).
    public long? ReplyToId { get; set; }

    // The users named by <@id> tokens in Text, validated against users when the text was written.
    public long[] MentionIds { get; set; } = [];

    // The literal words @everyone / @here, kept only when the sender held MENTION_EVERYONE in the
    // channel at the time; without the bit they stay plain text and both flags are false.
    public bool MentionEveryone { get; set; }

    public bool MentionHere { get; set; }
}
