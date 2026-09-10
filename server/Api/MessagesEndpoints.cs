using System.Globalization;
using Vorcall.Server.Chat;

namespace Vorcall.Server.Api;

public static class MessagesEndpoints
{
    private const int DefaultLimit = 100;
    private const int MinLimit = 1;
    private const int MaxLimit = 100;

    // GET /api/messages?room=&limit=&before= -> MessagePage as application/x-protobuf.
    public static async Task<IResult> GetPageAsync(string? room, string? limit, string? before, MessageService messages)
    {
        if (!Validation.TryNormalizeRoomId(room, out var roomId))
        {
            return Results.BadRequest();
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
