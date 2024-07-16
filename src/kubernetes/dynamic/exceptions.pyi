class DynamicApiError(Exception): ...  # TODO: Supposed to be kubernetes.client.rest.ApiException

class ResourceNotFoundError(Exception):
    """Resource was not found in available APIs"""

class ResourceNotUniqueError(Exception):
    """Parameters given matched multiple API resources"""

class KubernetesValidateMissing(Exception):
    """kubernetes-validate is not installed"""

# HTTP Errors
class BadRequestError(DynamicApiError):
    """400: StatusBadRequest"""

class UnauthorizedError(DynamicApiError):
    """401: StatusUnauthorized"""

class ForbiddenError(DynamicApiError):
    """403: StatusForbidden"""

class NotFoundError(DynamicApiError):
    """404: StatusNotFound"""

class MethodNotAllowedError(DynamicApiError):
    """405: StatusMethodNotAllowed"""

class ConflictError(DynamicApiError):
    """409: StatusConflict"""

class GoneError(DynamicApiError):
    """410: StatusGone"""

class UnprocessibleEntityError(DynamicApiError):
    """422: StatusUnprocessibleEntity"""

class TooManyRequestsError(DynamicApiError):
    """429: StatusTooManyRequests"""

class InternalServerError(DynamicApiError):
    """500: StatusInternalServer"""

class ServiceUnavailableError(DynamicApiError):
    """503: StatusServiceUnavailable"""

class ServerTimeoutError(DynamicApiError):
    """504: StatusServerTimeout"""
