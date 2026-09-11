using System.Globalization;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;

namespace Vorcall.Server.Api;

public static class MessagesEndpoints
{
    private const int DefaultLimit = 100;
    private const int MinLimit = 1;
    private const int MaxLimit = 100;

    // GET /api/messages?room=&limit=&before= -> MessagePage as application/x-protobuf.
    public static async Task<IResult> GetPageAsync(
        string? room,
        string? limit,
        string? before,
        HttpContext context,
        ConnectionRegistry registry,
        MessageService messages)
    {
        if (!Validation.TryNormalizeRoomId(room, out var roomId))
        {
            return Results.BadRequest();
        }

        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // A room the caller is not in and a room that does not exist answer alike: the reader
        // learns nothing about rooms it was never told about.
        if (!registry.IsMember(roomId, userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, "not a member");
        }

        var pageSize = DefaultLimit;
        if (limit is not null)
        {
            if (!int.TryParse(limit, NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsedLimit))
            {
                return Results.BadRequest();
            }

            pageSize = Math.Clamp(parsedLimit, MinLimit, MaxLimit);
        }

        long? exclusiveUpperBound = null;
        if (before is not null)
        {
            if (!long.TryParse(before, NumberStyles.Integer, CultureInfo.InvariantCulture, out var parsedBefore) || parsedBefore < 1)
            {
                return Results.BadRequest();
            }

            exclusiveUpperBound = parsedBefore;
        }

        var page = await messages.GetPageAsync(roomId, pageSize, exclusiveUpperBound);
        return ProtobufBody.Proto(page);
    }
}
