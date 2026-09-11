namespace Vorcall.Server.Data;

// One account's reaction with one emoji. The wire groups these by emoji; the rows do not.
public class Reaction
{
    public long MessageId { get; set; }

    public long UserId { get; set; }

    public string Emoji { get; set; } = string.Empty;

    // Orders the emoji groups on the wire by when each emoji was first used on the message.
    public DateTime CreatedAt { get; set; }
}
