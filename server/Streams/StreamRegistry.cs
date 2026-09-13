using System.Diagnostics.CodeAnalysis;

namespace Vorcall.Server.Streams;

// How a reader's wait for the owning client ended.
public enum TransferAnswer
{
    // The owner's chunk push arrived and holds the transfer.
    Claimed,

    // The owner will not serve this range; PendingTransfer.DeclineReason says why.
    Declined,

    // The owner's session ended before it answered.
    OwnerOffline,
}

// Whether an owner's answer — a chunk push or a decline — found the transfer it names. Unknown
// covers an id nothing has, one that belongs to another account and one about another stream
// alike: an answer to somebody else's transfer learns nothing about it.
public enum AnswerStatus
{
    Unknown,
    AlreadyClaimed,
    Accepted,
}

// Transfer is set only when Status is Accepted.
public sealed record ClaimOutcome(AnswerStatus Status, PendingTransfer? Transfer)
{
    public static ClaimOutcome Unknown { get; } = new(AnswerStatus.Unknown, null);

    public static ClaimOutcome AlreadyClaimed { get; } = new(AnswerStatus.AlreadyClaimed, null);

    public static ClaimOutcome Accepted(PendingTransfer transfer) => new(AnswerStatus.Accepted, transfer);
}

// One reader's request for one range of one streamed file, from the StreamRequest that asks the
// owner for it to the last byte of the owner's push. The reader's GET and the owner's POST are two
// HTTP requests that never see each other, and the three tasks here are where they meet: the GET
// awaits Answered, then hands its response body over and awaits Done; the POST awaits Body and
// copies into it. All three run their continuations asynchronously, because each is completed
// either under the registry's gate or on the other request's thread, and neither is a place to
// run a copy loop from.
public sealed class PendingTransfer
{
    private readonly TaskCompletionSource<TransferAnswer> _answered = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource<Stream> _body = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly TaskCompletionSource<bool> _done = new(TaskCreationOptions.RunContinuationsAsynchronously);
    private readonly CancellationTokenSource _cancellation = new();

    internal PendingTransfer(long id, long streamId, long ownerId, long offset, long length)
    {
        Id = id;
        StreamId = streamId;
        OwnerId = ownerId;
        Offset = offset;
        Length = length;
    }

    public long Id { get; }

    public long StreamId { get; }

    public long OwnerId { get; }

    public long Offset { get; }

    public long Length { get; }

    // Set by the decline that answered; empty otherwise.
    public string DeclineReason { get; private set; } = string.Empty;

    public Task<TransferAnswer> Answered => _answered.Task;

    // The reader's response body, once the reader has its status and headers on the wire.
    public Task<Stream> Body => _body.Task;

    // True when every byte of the range went through.
    public Task<bool> Done => _done.Task;

    // Cancelled when the transfer is abandoned: the reader left, its wait ran out, or the host is
    // stopping. The owner's push copies under it.
    public CancellationToken Cancellation => _cancellation.Token;

    // Read and written under StreamRegistry's gate only.
    internal bool Claimed { get; private set; }

    public void HandOver(Stream body) => _body.TrySetResult(body);

    internal void Claim()
    {
        Claimed = true;
        _answered.TrySetResult(TransferAnswer.Claimed);
    }

    internal void Decline(string reason)
    {
        DeclineReason = reason;
        _answered.TrySetResult(TransferAnswer.Declined);
    }

    internal void OwnerWentOffline() => _answered.TrySetResult(TransferAnswer.OwnerOffline);

    internal void Complete(bool whole) => _done.TrySetResult(whole);

    internal void Abandon()
    {
        _cancellation.Cancel();
        _body.TrySetCanceled(_cancellation.Token);
    }
}

// The in-memory meeting point of the streamed-file proxy: every transfer a reader is waiting on,
// keyed by the transfer id the owner was sent. Nothing here touches the database or a socket —
// StreamsEndpoints drives it from both ends, and ConnectionRegistry tells it when an owner's
// session ends. The gate is never held across an await, and a cancel never runs under it: a
// CancellationTokenSource fires its callbacks inline, and the push's are not this class's to run.
public sealed class StreamRegistry(StreamOptions options)
{
    private readonly Lock _gate = new();
    private readonly Dictionary<long, PendingTransfer> _transfers = [];

    // Null when the owner already serves as many transfers as the options allow.
    public PendingTransfer? TryRegister(long ownerId, long streamId, long offset, long length)
    {
        lock (_gate)
        {
            var serving = 0;
            foreach (var transfer in _transfers.Values)
            {
                if (transfer.OwnerId == ownerId)
                {
                    serving++;
                }
            }

            if (serving >= options.MaxTransfersPerOwner)
            {
                return null;
            }

            // Random rather than sequential: an id tells nobody how many transfers ran, and one a
            // client kept across a restart names nothing after it.
            long id;
            do
            {
                id = Random.Shared.NextInt64(1, long.MaxValue);
            }
            while (_transfers.ContainsKey(id));

            var pending = new PendingTransfer(id, streamId, ownerId, offset, length);
            _transfers[id] = pending;
            return pending;
        }
    }

    // The owner's chunk push taking the transfer. A transfer is claimed once: the second push for
    // the same id is refused, whoever sends it, and it stays here until the push completes so that
    // the refusal holds for as long as the copy runs.
    public ClaimOutcome Claim(long transferId, long ownerId, long streamId)
    {
        lock (_gate)
        {
            if (!TryFindLocked(transferId, ownerId, streamId, out var transfer))
            {
                return ClaimOutcome.Unknown;
            }

            if (transfer.Claimed)
            {
                return ClaimOutcome.AlreadyClaimed;
            }

            transfer.Claim();
            return ClaimOutcome.Accepted(transfer);
        }
    }

    // The owner saying no. The transfer is gone from here on, so a push that follows the decline
    // finds nothing.
    public AnswerStatus Decline(long transferId, long ownerId, long streamId, string reason)
    {
        lock (_gate)
        {
            if (!TryFindLocked(transferId, ownerId, streamId, out var transfer))
            {
                return AnswerStatus.Unknown;
            }

            if (transfer.Claimed)
            {
                return AnswerStatus.AlreadyClaimed;
            }

            _transfers.Remove(transferId);
            transfer.Decline(reason);
            return AnswerStatus.Accepted;
        }
    }

    // The owner's push ended; whole says whether every byte of the range went through.
    public void Complete(PendingTransfer transfer, bool whole)
    {
        lock (_gate)
        {
            _transfers.Remove(transfer.Id);
        }

        transfer.Complete(whole);
    }

    // The reader is gone — it hung up, its wait ran out, or the host is stopping — so the owner's
    // push, running or still to come, has nowhere to go.
    public void Abandon(PendingTransfer transfer)
    {
        lock (_gate)
        {
            _transfers.Remove(transfer.Id);
        }

        transfer.Abandon();
    }

    // The owner's session ended. A StreamRequest queued at it is never answered now, so the
    // readers waiting on one are told so rather than left to their timeout. A push already under
    // way rides its own HTTP request and needs nothing from the socket, so it is left alone.
    public void FaultOwner(long ownerId)
    {
        List<PendingTransfer>? faulted = null;
        lock (_gate)
        {
            foreach (var transfer in _transfers.Values)
            {
                if (transfer.OwnerId == ownerId && !transfer.Claimed)
                {
                    (faulted ??= []).Add(transfer);
                }
            }

            if (faulted is null)
            {
                return;
            }

            foreach (var transfer in faulted)
            {
                _transfers.Remove(transfer.Id);
            }
        }

        foreach (var transfer in faulted)
        {
            transfer.OwnerWentOffline();
            transfer.Abandon();
        }
    }

    private bool TryFindLocked(long transferId, long ownerId, long streamId, [NotNullWhen(true)] out PendingTransfer? transfer)
    {
        if (_transfers.TryGetValue(transferId, out var found) && found.OwnerId == ownerId && found.StreamId == streamId)
        {
            transfer = found;
            return true;
        }

        transfer = null;
        return false;
    }
}
