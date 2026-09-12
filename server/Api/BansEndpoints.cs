using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

// The ban list, for the moderation screen. Banning and unbanning are WebSocket frames: they
// broadcast, so they belong on the connection rather than here.
public static class BansEndpoints
{
    public static void Map(WebApplication app) => app.MapGet("/api/bans", ListAsync).RequireAuthorization();

    // GET /api/bans -> BanList.
    private static async Task<IResult> ListAsync(HttpContext context, ConnectionRegistry registry, MemberDirectory members)
    {
        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Server-scoped bit, hence the null channel.
        if (!registry.Has(userId, null, Perm.BanMembers))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.BanMembers));
        }

        var list = new BanList();
        foreach (var record in await members.ListBansAsync(context.RequestAborted))
        {
            list.Bans.Add(new Ban
            {
                UserId = record.UserId,
                Username = record.Username,
                Reason = record.Reason,

                // 0 once the account that issued the ban has been deleted.
                BannedBy = record.BannedBy ?? 0,
                BannedAtUnixMs = new DateTimeOffset(record.BannedAt).ToUnixTimeMilliseconds(),
            });
        }

        return ProtobufBody.Proto(list);
    }
}
