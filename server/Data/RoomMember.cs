namespace Vorcall.Server.Data;

// One account's persistent membership of one room. Distinct from presence, which is per live
// connection and never touches the database.
public class RoomMember
{
    public string RoomId { get; set; } = string.Empty;

    public long UserId { get; set; }

    public DateTime JoinedAt { get; set; }

    // The read cursor: everything above it in this room is unread for this member.
    public long LastReadMessageId { get; set; }
}
