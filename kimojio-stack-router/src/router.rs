// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Method and path routing.

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt;
use std::marker::PhantomData;
use std::sync::Arc;

use http::{Method, Request, Response};
use kimojio_stack_http::Body;
use kimojio_stack_tower::{Readiness, Service};

use crate::extract::SharedState;
use crate::{Handler, PathParams, Rejection, handler_fn};

/// Router construction error.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RouteError {
    message: String,
}

impl RouteError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for RouteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for RouteError {}

/// Method-aware handler builder for one path.
pub struct MethodRouter<Cx> {
    routes: Vec<(Method, Box<dyn Handler<Cx>>)>,
}

impl<Cx> MethodRouter<Cx> {
    /// Creates an empty method router.
    pub fn new() -> Self {
        Self { routes: Vec::new() }
    }

    /// Adds a handler for one HTTP method.
    pub fn on(mut self, method: Method, handler: impl Handler<Cx> + 'static) -> Self {
        self.routes.push((method, Box::new(handler)));
        self
    }

    /// Adds a GET handler.
    pub fn get(self, handler: impl Handler<Cx> + 'static) -> Self {
        self.on(Method::GET, handler)
    }

    /// Adds a POST handler.
    pub fn post(self, handler: impl Handler<Cx> + 'static) -> Self {
        self.on(Method::POST, handler)
    }

    /// Adds a PUT handler.
    pub fn put(self, handler: impl Handler<Cx> + 'static) -> Self {
        self.on(Method::PUT, handler)
    }

    /// Adds a DELETE handler.
    pub fn delete(self, handler: impl Handler<Cx> + 'static) -> Self {
        self.on(Method::DELETE, handler)
    }
}

impl<Cx> Default for MethodRouter<Cx> {
    fn default() -> Self {
        Self::new()
    }
}

/// Stackful HTTP router.
pub struct Router<Cx> {
    routes: Vec<Route<Cx>>,
    fallback: Box<dyn Handler<Cx>>,
    state: Option<Arc<dyn Any + Send + Sync>>,
    _cx: PhantomData<fn(&Cx)>,
}

impl<Cx> Router<Cx> {
    /// Creates an empty router with a 404 fallback.
    pub fn new() -> Self {
        Self {
            routes: Vec::new(),
            fallback: Box::new(handler_fn(|_: &Cx, _| {
                Ok::<_, Rejection>(Rejection::not_found().into_response())
            })),
            state: None,
            _cx: PhantomData,
        }
    }

    /// Adds a single method route.
    pub fn route(
        mut self,
        path: &str,
        method: Method,
        handler: impl Handler<Cx> + 'static,
    ) -> Result<Self, RouteError> {
        self.push_route(path, method, Box::new(handler))?;
        Ok(self)
    }

    /// Adds all method routes from a [`MethodRouter`].
    pub fn route_methods(
        mut self,
        path: &str,
        methods: MethodRouter<Cx>,
    ) -> Result<Self, RouteError> {
        for (method, handler) in methods.routes {
            self.push_route(path, method, handler)?;
        }
        Ok(self)
    }

    /// Sets fallback handler.
    pub fn fallback(mut self, handler: impl Handler<Cx> + 'static) -> Self {
        self.fallback = Box::new(handler);
        self
    }

    /// Adds shared application state.
    pub fn with_state<T>(mut self, state: T) -> Self
    where
        T: Send + Sync + 'static,
    {
        self.state = Some(Arc::new(state));
        self
    }

    /// Merges another router into this router.
    pub fn merge(mut self, other: Router<Cx>) -> Result<Self, RouteError> {
        for route in other.routes {
            self.push_existing_route(route)?;
        }
        Ok(self)
    }

    /// Nests another router under `prefix`.
    pub fn nest(mut self, prefix: &str, other: Router<Cx>) -> Result<Self, RouteError> {
        let prefix = normalize_prefix(prefix)?;
        for mut route in other.routes {
            route.pattern = route.pattern.prefixed(&prefix);
            self.push_existing_route(route)?;
        }
        Ok(self)
    }

    fn push_route(
        &mut self,
        path: &str,
        method: Method,
        handler: Box<dyn Handler<Cx>>,
    ) -> Result<(), RouteError> {
        let route = Route {
            method,
            pattern: RoutePattern::parse(path)?,
            handler,
        };
        self.push_existing_route(route)
    }

    fn push_existing_route(&mut self, route: Route<Cx>) -> Result<(), RouteError> {
        if self.routes.iter().any(|existing| {
            existing.method == route.method && existing.pattern.shape() == route.pattern.shape()
        }) {
            return Err(RouteError::new(format!(
                "duplicate or ambiguous route {} {}",
                route.method,
                route.pattern.display()
            )));
        }
        self.routes.push(route);
        Ok(())
    }

    /// Handles one request through this router.
    pub fn handle(
        &mut self,
        cx: &Cx,
        mut request: Request<Body>,
    ) -> Result<Response<Body>, Rejection> {
        if let Some(state) = &self.state {
            request
                .extensions_mut()
                .insert(SharedState(Arc::clone(state)));
        }

        let mut method_not_allowed = false;
        let mut best_match = None;
        for (index, route) in self.routes.iter().enumerate() {
            let Some(params) = route.pattern.matches(request.uri().path()) else {
                continue;
            };
            if route.method != *request.method() {
                method_not_allowed = true;
                continue;
            }
            let priority = route.pattern.priority();
            if best_match
                .as_ref()
                .is_none_or(|(_, best_priority, _)| priority > *best_priority)
            {
                best_match = Some((index, priority, params));
            }
        }

        if let Some((index, _, params)) = best_match {
            request.extensions_mut().insert(params);
            return self.routes[index].handler.call(cx, request);
        }

        if method_not_allowed {
            Err(Rejection::method_not_allowed())
        } else {
            self.fallback.call(cx, request)
        }
    }
}

impl<Cx> Default for Router<Cx> {
    fn default() -> Self {
        Self::new()
    }
}

impl<Cx> Service<Cx, Request<Body>> for Router<Cx> {
    type Response = Response<Body>;
    type Error = Rejection;

    fn ready(&mut self, _cx: &Cx) -> Result<Readiness, Self::Error> {
        Ok(Readiness::Ready)
    }

    fn call(&mut self, cx: &Cx, request: Request<Body>) -> Result<Self::Response, Self::Error> {
        self.handle(cx, request)
    }
}

struct Route<Cx> {
    method: Method,
    pattern: RoutePattern,
    handler: Box<dyn Handler<Cx>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RoutePattern {
    segments: Vec<Segment>,
}

impl RoutePattern {
    fn parse(path: &str) -> Result<Self, RouteError> {
        if !path.starts_with('/') {
            return Err(RouteError::new("route path must start with '/'"));
        }
        let mut names = BTreeMap::new();
        let mut segments = Vec::new();
        for segment in path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
        {
            let segment = if let Some(name) = segment.strip_prefix(':') {
                if name.is_empty() {
                    return Err(RouteError::new("path parameter name cannot be empty"));
                }
                if names.insert(name.to_owned(), ()).is_some() {
                    return Err(RouteError::new(format!("duplicate path parameter {name}")));
                }
                Segment::Param(name.to_owned())
            } else {
                Segment::Literal(segment.to_owned())
            };
            segments.push(segment);
        }
        Ok(Self { segments })
    }

    fn prefixed(self, prefix: &RoutePattern) -> Self {
        let mut segments = prefix.segments.clone();
        segments.extend(self.segments);
        Self { segments }
    }

    fn matches(&self, path: &str) -> Option<PathParams> {
        let path_segments = path
            .trim_matches('/')
            .split('/')
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>();
        if path_segments.len() != self.segments.len() {
            return None;
        }
        let mut values = BTreeMap::new();
        for (pattern, actual) in self.segments.iter().zip(path_segments) {
            match pattern {
                Segment::Literal(expected) if expected == actual => {}
                Segment::Literal(_) => return None,
                Segment::Param(name) => {
                    values.insert(name.clone(), actual.to_owned());
                }
            }
        }
        Some(PathParams::new(values))
    }

    fn shape(&self) -> String {
        self.segments
            .iter()
            .map(|segment| match segment {
                Segment::Literal(value) => format!("={value}"),
                Segment::Param(_) => ":".to_owned(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    fn display(&self) -> String {
        if self.segments.is_empty() {
            return "/".to_owned();
        }
        let mut path = String::new();
        for segment in &self.segments {
            path.push('/');
            match segment {
                Segment::Literal(value) => path.push_str(value),
                Segment::Param(name) => {
                    path.push(':');
                    path.push_str(name);
                }
            }
        }
        path
    }

    fn priority(&self) -> Vec<u8> {
        self.segments
            .iter()
            .map(|segment| u8::from(matches!(segment, Segment::Literal(_))))
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Segment {
    Literal(String),
    Param(String),
}

fn normalize_prefix(prefix: &str) -> Result<RoutePattern, RouteError> {
    if prefix == "/" {
        return Ok(RoutePattern {
            segments: Vec::new(),
        });
    }
    RoutePattern::parse(prefix)
}
