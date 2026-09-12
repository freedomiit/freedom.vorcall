namespace Vorcall.Server.Data;

// Server-side only: the wire carries a role id and a user id of which exactly one is non-zero.
public enum OverrideTarget
{
    Role = 1,
    User = 2,
}

// One channel's allow/deny pair for one role or one member. Only channel-scoped bits are ever
// stored; the server-scoped ones are masked away on write.
public class ChannelOverride
{
    public long ChannelId { get; set; }

    public OverrideTarget TargetKind { get; set; }

    public long TargetId { get; set; }

    public long Allow { get; set; }

    public long Deny { get; set; }
}
