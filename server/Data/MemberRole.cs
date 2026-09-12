namespace Vorcall.Server.Data;

// One account holding one role. The everyone role is implicit and never stored here.
public class MemberRole
{
    public long UserId { get; set; }

    public long RoleId { get; set; }
}
