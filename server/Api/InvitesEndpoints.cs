using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;
using Vorcall.Server.Protocol;

namespace Vorcall.Server.Api;

// Invite administration: the list, minting one, and revoking one that nobody used. MANAGE_INVITES
// on every call, and the plaintext code exists only in the 201 of the call that created it — the
// server keeps its hash and no line here ever logs a code.
public static class InvitesEndpoints
{
    private const string DaysField = "days";

    public static void Map(WebApplication app)
    {
        app.MapGet("/api/invites", ListAsync).RequireAuthorization();
        app.MapPost("/api/invites", CreateAsync).RequireAuthorization();
        app.MapDelete("/api/invites/{id:long}", RevokeAsync).RequireAuthorization();
    }

    // GET /api/invites -> InviteList, used invites included.
    private static async Task<IResult> ListAsync(HttpContext context, ConnectionRegistry registry, InviteService invites)
    {
        if (Refuse(context, registry, out _) is { } refusal)
        {
            return refusal;
        }

        var list = new InviteList();
        foreach (var record in await invites.ListAsync(context.RequestAborted))
        {
            list.Invites.Add(new Invite
            {
                Id = record.Id,
                CreatedAtUnixMs = UnixMs(record.CreatedAt),
                ExpiresAtUnixMs = UnixMs(record.ExpiresAt),

                // 0 is the wire's "still unused"; an empty username also covers an account that
                // used the invite and has since been deleted.
                UsedBy = record.UsedBy ?? 0,
                UsedByUsername = record.UsedByUsername ?? string.Empty,

                // 0 for an invite the admin CLI minted, which has no account behind it.
                CreatedBy = record.CreatedBy ?? 0,
            });
        }

        return ProtobufBody.Proto(list);
    }

    // POST /api/invites, body CreateInviteRequest -> 201 InviteCreated.
    private static async Task<IResult> CreateAsync(HttpContext context, ConnectionRegistry registry, InviteService invites)
    {
        if (Refuse(context, registry, out var userId) is { } refusal)
        {
            return refusal;
        }

        var body = await ProtobufBody.ReadAsync(context, CreateInviteRequest.Parser);
        if (body.Message is not { } request)
        {
            return body.Failure;
        }

        // Unsigned on the wire, so the upper bound is compared before the narrowing: a value past
        // int.MaxValue must not wrap into a valid day count.
        if (request.Days > InviteService.MaxDays || !InviteService.IsValidDays((int)request.Days))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, DaysField);
        }

        var created = await invites.CreateAsync((int)request.Days, userId, DateTime.UtcNow, context.RequestAborted);
        return ProtobufBody.Proto(
            new InviteCreated
            {
                Id = created.Id,
                Code = created.Code,
                ExpiresAtUnixMs = UnixMs(created.ExpiresAt),
            },
            StatusCodes.Status201Created);
    }

    // DELETE /api/invites/{id} -> 204.
    private static async Task<IResult> RevokeAsync(HttpContext context, long id, ConnectionRegistry registry, InviteService invites)
    {
        if (Refuse(context, registry, out _) is { } refusal)
        {
            return refusal;
        }

        if (await invites.RevokeAsync(id, context.RequestAborted))
        {
            return Results.NoContent();
        }

        // A used invite keeps its row as the audit trail of the account it let in, and it cannot be
        // handed out again either way, so the caller gets what it asked for. Only an id that names
        // no invite at all is a 404.
        return await invites.ExistsAsync(id, context.RequestAborted)
            ? Results.NoContent()
            : ProtobufBody.Fail(StatusCodes.Status404NotFound, "no such invite");
    }

    // Null when the caller may proceed; userId is meaningful only then.
    private static IResult? Refuse(HttpContext context, ConnectionRegistry registry, out long userId)
    {
        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // Server-scoped bit, hence the null channel.
        return registry.Has(userId, null, Perm.ManageInvites)
            ? null
            : ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ManageInvites));
    }

    private static long UnixMs(DateTime value) => new DateTimeOffset(value).ToUnixTimeMilliseconds();
}
