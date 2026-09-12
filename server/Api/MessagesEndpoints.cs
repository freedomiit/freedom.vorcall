using System.Globalization;
using Vorcall.Server.Auth;
using Vorcall.Server.Chat;
using Vorcall.Server.Permissions;

namespace Vorcall.Server.Api;

public static class MessagesEndpoints
{
    private const int DefaultLimit = 100;
    private const int MinLimit = 1;
    private const int MaxLimit = 100;

    // The query parameter a 400 names, so a client can point at the field that was wrong.
    private const string ChannelField = "channel";

    // GET /api/messages?channel=&limit=&before= -> MessagePage as application/x-protobuf.
    public static async Task<IResult> GetPageAsync(
        string? channel,
        string? limit,
        string? before,
        HttpContext context,
        ConnectionRegistry registry,
        MessageService messages)
    {
        if (!Validation.TryParseChannelId(channel, out var channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
        }

        // The token validated, so a missing claim is this server's own bug.
        if (!BearerIdentity.TryGetUserId(context.User, out var userId))
        {
            return ProtobufBody.Fail(StatusCodes.Status401Unauthorized, "invalid bearer");
        }

        // A channel the caller may not view and a channel that does not exist answer alike: the
        // reader learns nothing about channels it was never told about.
        if (registry.ChannelOf(channelId) is not { } info || !registry.CanView(userId, channelId))
        {
            return ProtobufBody.Fail(StatusCodes.Status403Forbidden, PermNames.Name(Perm.ViewChannel));
        }

        // A voice channel holds no messages at all, so asking for its history is a malformed
        // request rather than one this caller is not allowed to make.
        if (info.Kind == Data.ChannelKind.Voice)
        {
            return ProtobufBody.Fail(StatusCodes.Status400BadRequest, ChannelField);
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

        var page = await messages.GetPageAsync(channelId, pageSize, exclusiveUpperBound);
        return ProtobufBody.Proto(page);
    }
}
